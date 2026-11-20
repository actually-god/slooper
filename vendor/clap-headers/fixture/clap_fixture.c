// slooper CLAP host test fixture: two deterministic plugins in one DSO.
//
//   com.slooper.fixture-gain — stereo effect: out = in * gain (param "gain",
//                              plain 0..1, default 1.0).
//   com.slooper.fixture-sine — note-effect (instrument): NOTE_ON starts an
//                              ideal sine at 440 Hz (key A4; other keys
//                              follow 12-TET) with amplitude 0.5 * velocity
//                              * amp, param "amp" plain 0..1 default 1.0.
//
// Both implement clap.params, clap.audio-ports, clap.state so scan, param
// relay, audio and state round-trips are all pinned against known output.
// State is plain text: "F1 <gain> <amp>" (load tolerates missing fields).
// v79: the GAIN plugin additionally implements clap_plugin_preset_load
// (from_location) and the DSO exports a preset-discovery factory; the
// sine plugin deliberately does NOT (negative-polarity control).
//
// This file is test infrastructure for src/claphost.rs; MIT like the host.

#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#ifdef _WIN32
#include <windows.h>
// Windows parity fix (v79 CI run 44160764104): the host pins set
// SLOOPER_CLAP_FIXTURE_PRESET_DIR via Rust set_var -> SetEnvironmentVariableW
// (Win32 env). UCRT getenv() here would read the CRT _environ SNAPSHOT taken
// at process start and NEVER see it. Query the live Win32 env instead.
static char fix_env_buf[4096];
static const char *fix_getenv(const char *name) {
   DWORD n = GetEnvironmentVariableA(name, fix_env_buf, (DWORD)sizeof(fix_env_buf) - 1);
   if (n == 0 || n >= sizeof(fix_env_buf) - 1) return NULL;
   fix_env_buf[n] = '\0';
   return fix_env_buf;
}
#else
static const char *fix_getenv(const char *name) { return getenv(name); }
#endif

#include <clap/clap.h>

#define SINE_VOICES 8
#define TWO_PI 6.2831853071795864769

typedef struct voice {
   int32_t key;    // -1 = free
   double  phase;  // 0..1
   double  amp;    // linear, velocity-scaled
} voice_t;

typedef struct fixture_plugin {
   clap_plugin_t             plugin;
   const clap_host_t        *host;
   bool                      is_sine; // select behaviour by descriptor id

   double                    gain; // plain 0..1 (effect)
   double                    amp;  // plain 0..1 (synth)
   bool                      load_armed;    // v67: fixload latch
   bool                      load_reported; // v67: sink answered loaded()

   // v103 (card t_62c5a14e): raw-MIDI observer — the host rescales
   // roll-space pitch bends to the track range and announces RPN0; pins
   // read the results back through the hidden probe params (ids 4/5).
   double                    bend_val;  // last pitch bend, 14-bit unsigned
   int                       rpn_sel;   // last CC101 (RPN MSB select)
   int                       rpn_act;   // slot 0/0 currently selected
   double                    rpn_msb;   // last CC6 while slot 0/0 active
   int                       midi_msgs; // raw MIDI events seen

   double                    sample_rate;
   bool                      active;
   bool                      processing;

   voice_t                   voices[SINE_VOICES];
} fixture_plugin_t;

static fixture_plugin_t *selfp(const clap_plugin_t *p) { return (fixture_plugin_t *)p->plugin_data; }

//////////////////////////  plugin core  //////////////////////////////////

static bool fx_init(const clap_plugin_t *p) { return true; }
static void fx_destroy(const clap_plugin_t *p) { free((void *)p->plugin_data); }

static bool fx_activate(const clap_plugin_t *p, double sr, uint32_t lo, uint32_t hi) {
   fixture_plugin_t *f = selfp(p);
   (void)lo; (void)hi;
   f->sample_rate = sr > 1.0 ? sr : 48000.0;
   f->active = true;
   return true;
}
static void fx_deactivate(const clap_plugin_t *p) { selfp(p)->active = false; }
static bool fx_start(const clap_plugin_t *p) { selfp(p)->processing = true; return true; }
static void fx_stop(const clap_plugin_t *p) { selfp(p)->processing = false; }
static void fx_reset(const clap_plugin_t *p) {
   fixture_plugin_t *f = selfp(p);
   for (int i = 0; i < SINE_VOICES; i++) f->voices[i].key = -1;
}

static bool fx_note_event(const clap_plugin_t *p, const clap_event_header_t *h) {
   fixture_plugin_t *f = selfp(p);
   if (h->type != CLAP_EVENT_NOTE_ON && h->type != CLAP_EVENT_NOTE_OFF)
      return false;
   const clap_event_note_t *n = (const clap_event_note_t *)h;
   if (h->type == CLAP_EVENT_NOTE_ON) {
      voice_t *v = NULL;
      for (int i = 0; i < SINE_VOICES; i++)
         if (f->voices[i].key == n->key) { v = &f->voices[i]; break; }
      if (!v)
         for (int i = 0; i < SINE_VOICES; i++)
            if (f->voices[i].key < 0) { v = &f->voices[i]; break; }
      if (v) {
         v->key = n->key;
         v->phase = 0.0;
         v->amp = 0.5 * n->velocity;
      }
   } else {
      for (int i = 0; i < SINE_VOICES; i++)
         if (f->voices[i].key == n->key) f->voices[i].key = -1;
   }
   return true;
}

static void fx_apply_param(const clap_plugin_t *p, const clap_event_param_value_t *v) {
   fixture_plugin_t *f = selfp(p);
   if (v->param_id == 1) f->gain = v->value;
   else if (v->param_id == 2) f->amp = v->value;
   else if (v->param_id == 3 && v->value >= 0.5 && !f->load_armed) {
      // v67: "load one of my presets" - report through the host's
      // clap.preset-load sink, then mutate our own gain so a chunk
      // harvest proves the host captured a plugin-side change.
      // Spec says loaded() runs on the MAIN thread; the fixture fires
      // from the param event because that is our deterministic hook,
      // and the slooper sink is a mutex store (thread-safe by design).
      f->load_armed = true;
      const clap_host_preset_load_t *pl = NULL;
      if (f->host && f->host->get_extension)
         pl = (const clap_host_preset_load_t *)f->host->get_extension(
             f->host, CLAP_EXT_PRESET_LOAD);
      if (pl && pl->loaded) {
         // location_kind 1 = PLUGIN_PRESET (the clap.preset-discovery/2
         // enum lives in a factory header this fixture does not include).
         pl->loaded(f->host, 1, "slooper-fixture-factory-preset", NULL);
         f->load_reported = true;
      }
      f->gain = 0.75;
   }
}

static clap_process_status fx_process(const clap_plugin_t *p, const clap_process_t *proc) {
   fixture_plugin_t *f = selfp(p);
   if (!f->active || !f->processing)
      return CLAP_PROCESS_ERROR;

   // Drain the event list in sample order.
   for (uint32_t i = 0; i < proc->in_events->size(proc->in_events); i++) {
      const clap_event_header_t *h = proc->in_events->get(proc->in_events, i);
      if (h->space_id != CLAP_CORE_EVENT_SPACE_ID)
         continue;
      switch (h->type) {
      case CLAP_EVENT_NOTE_ON:
      case CLAP_EVENT_NOTE_OFF:
         if (f->is_sine) fx_note_event(p, h);
         break;
      case CLAP_EVENT_PARAM_VALUE:
         fx_apply_param(p, (const clap_event_param_value_t *)h);
         break;
      case CLAP_EVENT_MIDI: {
         const clap_event_midi_t *mm = (const clap_event_midi_t *)h;
         const uint8_t *d = mm->data;
         fixture_plugin_t *f2 = selfp(p);
         f2->midi_msgs++;
         int status = d[0] & 0xF0;
         if (status == 0xE0)
            f2->bend_val = (double)(((d[2] << 7) | d[1]));
         else if (status == 0xB0) {
            int cc = d[1], v = d[2];
            if (cc == 101)
               f2->rpn_sel = v;
            else if (cc == 100)
               f2->rpn_act = (f2->rpn_sel == 0 && v == 0);
            else if (cc == 6 && f2->rpn_act)
               f2->rpn_msb = v;
         }
         break;
      }
      default:
         break;
      }
   }

   uint32_t frames = proc->frames_count;
   if (!f->is_sine) {
      // gain effect: pass through scaled (or silence when no input).
      const clap_audio_buffer_t *in = &proc->audio_inputs[0];
      clap_audio_buffer_t *out = &proc->audio_outputs[0];
      for (uint32_t c = 0; c < out->channel_count; c++) {
         float *dst = out->data32[c];
         const float *src = (proc->audio_inputs_count > 0) ? in->data32[c % in->channel_count] : NULL;
         (void)in;
         if (src)
            for (uint32_t i = 0; i < frames; i++) dst[i] = (float)(src[i] * f->gain);
         else
            for (uint32_t i = 0; i < frames; i++) dst[i] = 0.f;
      }
      return CLAP_PROCESS_CONTINUE;
   }

   clap_audio_buffer_t *out = &proc->audio_outputs[0];
   for (uint32_t c = 0; c < out->channel_count; c++)
      for (uint32_t i = 0; i < frames; i++) out->data32[c][i] = 0.f;

   double inc[8]; bool live[8];
   for (int v = 0; v < SINE_VOICES; v++) {
      voice_t *vo = &f->voices[v];
      live[v] = vo->key >= 0;
      double hz = 440.0 * pow(2.0, ((double)vo->key - 69.0) / 12.0);
      inc[v] = hz / f->sample_rate;
   }
   if (out->channel_count > 0) {
      float *l = out->data32[0];
      float *r = out->channel_count > 1 ? out->data32[1] : NULL;
      for (uint32_t i = 0; i < frames; i++) {
         double acc = 0.0;
         for (int v = 0; v < SINE_VOICES; v++) {
            if (!live[v]) continue;
            voice_t *vo = &f->voices[v];
            acc += sin(TWO_PI * vo->phase) * vo->amp * f->amp;
            vo->phase += inc[v];
            if (vo->phase >= 1.0) vo->phase -= 1.0;
         }
         l[i] = (float)acc;
         if (r) r[i] = l[i];
      }
   }
   return CLAP_PROCESS_CONTINUE;
}

//////////////////////////  params  ///////////////////////////////////////

#define PARAM_COUNT 5 /* +2 hidden probe params (v103) */

static uint32_t pm_count(const clap_plugin_t *p) { (void)p; return PARAM_COUNT; }

static bool pm_get_info(const clap_plugin_t *p, uint32_t idx, clap_param_info_t *info) {
   (void)p;
   if (idx >= PARAM_COUNT) return false;
   memset(info, 0, sizeof(*info));
   if (idx == 0) {
      info->id = 1;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "fixgain");
      info->min_value = 0.0; info->max_value = 1.0; info->default_value = 1.0;
   } else if (idx == 1) {
      info->id = 2;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "fixamp");
      info->min_value = 0.0; info->max_value = 1.0; info->default_value = 1.0;
   } else {
      // v67: preset-load trigger. Reaching >= 0.5 makes the plugin call
      // the host's clap.preset-load sink ONCE and then mutate its own
      // gain - a deterministic stand-in for a preset loaded through the
      // plugin's own machinery.
      info->id = 3;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "fixload");
      info->min_value = 0.0; info->max_value = 1.0; info->default_value = 0.0;
   } else if (idx == 3) {
      // v103: HIDDEN read-back probes for the bend currency pins (never
      // knobs; the host must not write them — IS_READONLY + IS_HIDDEN).
      info->id = 4;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "probe-bend");
      info->min_value = 0.0; info->max_value = 16383.0; info->default_value = 0.0;
      info->flags = CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_READONLY;
   } else {
      info->id = 5;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "probe-rpn");
      info->min_value = 0.0; info->max_value = 127.0; info->default_value = 0.0;
      info->flags = CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_READONLY;
   }
   return true;
}

static bool pm_get_value(const clap_plugin_t *p, clap_id id, double *out) {
   fixture_plugin_t *f = selfp(p);
   if (id == 1) { *out = f->gain; return true; }
   if (id == 2) { *out = f->amp; return true; }
   if (id == 3) { *out = f->load_reported ? 1.0 : 0.0; return true; }
   if (id == 4) { *out = f->bend_val; return true; }
   if (id == 5) { *out = f->rpn_msb; return true; }
   return false;
}

static bool pm_value_to_text(const clap_plugin_t *p, clap_id id, double value, char *buf,
                             uint32_t cap) {
   (void)p; (void)id;
   snprintf(buf, cap, "%.4f", value);
   return true;
}

static bool pm_text_to_value(const clap_plugin_t *p, clap_id id, const char *text, double *out) {
   (void)p; (void)id;
   return sscanf(text, "%lf", out) == 1;
}

static void pm_flush(const clap_plugin_t *p, const clap_input_events_t *in,
                     const clap_output_events_t *out) {
   (void)out;
   for (uint32_t i = 0; i < in->size(in); i++) {
      const clap_event_header_t *h = in->get(in, i);
      if (h->type == CLAP_EVENT_PARAM_VALUE)
         fx_apply_param(p, (const clap_event_param_value_t *)h);
   }
}

static const clap_plugin_params_t s_params = {
   .count = pm_count,
   .get_info = pm_get_info,
   .get_value = pm_get_value,
   .value_to_text = pm_value_to_text,
   .text_to_value = pm_text_to_value,
   .flush = pm_flush,
};

//////////////////////////  audio ports  //////////////////////////////////

static uint32_t ap_count(const clap_plugin_t *p, bool is_input) {
   fixture_plugin_t *f = selfp(p);
   if (is_input) return f->is_sine ? 0 : 1;
   return 1;
}

static bool ap_get(const clap_plugin_t *p, uint32_t idx, bool is_input,
                   clap_audio_port_info_t *info) {
   if (idx > 0) return false;
   memset(info, 0, sizeof(*info));
   info->id = 0;
   snprintf(info->name, CLAP_NAME_SIZE, "%s", "st");
   info->channel_count = 2;
   info->flags = CLAP_AUDIO_PORT_IS_MAIN;
   info->port_type = CLAP_PORT_STEREO;
   info->in_place_pair = CLAP_INVALID_ID;
   (void)is_input;
   return true;
}

static const clap_plugin_audio_ports_t s_audio_ports = { .count = ap_count, .get = ap_get };

//////////////////////////  state  ////////////////////////////////////////

static bool st_save(const clap_plugin_t *p, const clap_ostream_t *out) {
   fixture_plugin_t *f = selfp(p);
   char buf[128];
   int n = snprintf(buf, sizeof(buf), "F1 %.9f %.9f\n", f->gain, f->amp);
   return out->write(out, buf, (uint64_t)n) == n;
}

// Read one whole line; returns false at EOF.
static bool read_line(const clap_istream_t *in, char *buf, uint32_t cap) {
   uint32_t k = 0;
   for (;;) {
      char c;
      int64_t r = in->read(in, &c, 1);
      if (r == 1) {
         if (c == '\n') { buf[k] = 0; return true; }
         if (k + 1 < cap) buf[k++] = c;
      } else {
         buf[k] = 0;
         return k > 0;
      }
   }
}

static bool st_load(const clap_plugin_t *p, const clap_istream_t *in) {
   fixture_plugin_t *f = selfp(p);
   char line[256];
   if (!read_line(in, line, sizeof(line))) return false;
   if (strncmp(line, "F1 ", 3) != 0) return false;
   double g = 1.0, a = 1.0;
   if (sscanf(line + 3, "%lf %lf", &g, &a) < 2) return false;
   f->gain = g;
   f->amp = a;
   return true;
}

static const clap_plugin_state_t s_state = { .save = st_save, .load = st_load };

static uint32_t lat_get(const clap_plugin_t *p) { (void)p; return 0; }
static const clap_plugin_latency_t s_latency = { .get = lat_get };

////////////////////  preset-load + discovery (v79)  ///////////////////////
//
// Host-commanded preset load (clap_plugin_preset-load/2.from_location)
// plus a minimal preset-discovery provider so the host has locations to
// load from. Preset-file grammar (fixture-defined): lines
// "FIXPRESET <key> [name...]" — a bare key (no name) is a single-file
// preset (reported with load_key NULL); key+name is a container entry.
// Keys: FA=0.25  FB=0.5  DEFAULT=0.375. A file with no FIXPRESET line
// must make the discovery receiver report on_error (the refusal path).

#define FIXPRESET_DIR_ENV "SLOOPER_CLAP_FIXTURE_PRESET_DIR"

static bool fix_apply_key(fixture_plugin_t *f, const char *key) {
   if (strcmp(key, "FA") == 0) f->gain = 0.25;
   else if (strcmp(key, "FB") == 0) f->gain = 0.5;
   else if (strcmp(key, "DEFAULT") == 0) f->gain = 0.375;
   else return false;
   return true;
}

static const clap_host_preset_load_t *fix_pl_sink(const fixture_plugin_t *f) {
   if (!f->host || !f->host->get_extension) return NULL;
   return (const clap_host_preset_load_t *)f->host->get_extension(
       f->host, CLAP_EXT_PRESET_LOAD);
}

// [main-thread] per spec: the host calls this on its control thread.
static bool pl_from_location(const clap_plugin_t *p, uint32_t kind,
                             const char *location, const char *load_key) {
   fixture_plugin_t *f = selfp(p);
   const char *key = (load_key && *load_key) ? load_key : NULL;
   bool ok = false;
   if (key) {
      ok = fix_apply_key(f, key);
   } else if (kind == 0 /*FILE*/ && location && *location) {
      FILE *fp = fopen(location, "rb");
      if (fp) {
         char line[256];
         while (fgets(line, sizeof(line), fp)) {
            char k[32];
            if (strncmp(line, "FIXPRESET ", 10) == 0 &&
                sscanf(line + 10, "%31s", k) == 1 && fix_apply_key(f, k)) {
               ok = true;
               break;
            }
         }
         fclose(fp);
      }
   }
   const clap_host_preset_load_t *pl = fix_pl_sink(f);
   // PLUGIN-kind success reports location=NULL (spec: plugin container).
   if (ok && pl && pl->loaded) pl->loaded(f->host, kind, location, key);
   if (!ok && pl && pl->on_error)
      pl->on_error(f->host, kind, location, key, 0,
                   key ? "fixture: unknown preset key"
                       : "fixture: no preset at location");
   return ok;
}

static const clap_plugin_preset_load_t s_preset_load = { pl_from_location };

static bool prov_init(const clap_preset_discovery_provider_t *prov) {
   const clap_preset_discovery_indexer_t *ix =
      (const clap_preset_discovery_indexer_t *)prov->provider_data;
   if (!ix || !ix->declare_filetype || !ix->declare_location) return false;
   clap_preset_discovery_filetype_t ft = { "fixture preset", "slooper fixture", "fxp" };
   ix->declare_filetype(ix, &ft);
   clap_preset_discovery_location_t loc_plugin = {
      CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT, "Fix Factory",
      CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN, NULL };
   ix->declare_location(ix, &loc_plugin);
   const char *dir = fix_getenv(FIXPRESET_DIR_ENV);
   if (dir && *dir) {
      clap_preset_discovery_location_t loc_files = {
         CLAP_PRESET_DISCOVERY_IS_USER_CONTENT, "Fixture files",
         CLAP_PRESET_DISCOVERY_LOCATION_FILE, dir };
      ix->declare_location(ix, &loc_files);
   }
   return true;
}

static void prov_destroy(const clap_preset_discovery_provider_t *prov) {
   free((void *)prov);
}

static bool prov_get_metadata(const clap_preset_discovery_provider_t *prov,
                              uint32_t kind, const char *location,
                              const clap_preset_discovery_metadata_receiver_t *recv) {
   (void)prov;
   if (!recv || !recv->begin_preset) return false;
   if (kind == CLAP_PRESET_DISCOVERY_LOCATION_PLUGIN) {
      clap_universal_plugin_id_t pid = { "clap", "com.slooper.fixture-gain" };
      if (recv->begin_preset(recv, "Fix Factory A", "FA")) {
         if (recv->add_plugin_id) recv->add_plugin_id(recv, &pid);
         if (recv->set_flags) recv->set_flags(recv, CLAP_PRESET_DISCOVERY_IS_FACTORY_CONTENT);
      }
      if (recv->begin_preset(recv, "Fix Factory B", "FB")) {
         if (recv->add_plugin_id) recv->add_plugin_id(recv, &pid);
      }
      return true;
   }
   FILE *fp = location ? fopen(location, "rb") : NULL;
   if (!fp) {
      if (recv->on_error) recv->on_error(recv, 2, "fixture: cannot open preset file");
      return false;
   }
   int found = 0;
   char line[256];
   while (fgets(line, sizeof(line), fp)) {
      char key[32], name[192], rest[160];
      if (strncmp(line, "FIXPRESET ", 10) != 0) continue;
      // "FIXPRESET <key> <name...>" -> container entry; bare key -> single
      if (sscanf(line + 10, "%31s %159[^\n]", key, rest) == 2) {
         snprintf(name, sizeof(name), "%s", rest);
         if (!recv->begin_preset(recv, name, key)) break;
      } else if (sscanf(line + 10, "%31s", key) == 1) {
         snprintf(name, sizeof(name), "Preset %s", key);
         if (!recv->begin_preset(recv, name, NULL)) break;
      } else {
         continue;
      }
      found++;
   }
   fclose(fp);
   if (!found) {
      if (recv->on_error) recv->on_error(recv, 0, "fixture: no presets in file");
      return false;
   }
   return true;
}

static const void *prov_get_ext(const clap_preset_discovery_provider_t *prov,
                                const char *id) {
   (void)prov; (void)id;
   return NULL;
}

static const clap_preset_discovery_provider_descriptor_t s_prov_desc = {
   CLAP_VERSION_INIT, "com.slooper.fixture-gain.presets",
   "Fixture preset provider", "slooper" };

static const clap_preset_discovery_provider_t *prov_create(
      const clap_preset_discovery_factory_t *factory,
      const clap_preset_discovery_indexer_t *indexer, const char *provider_id) {
   (void)factory;
   if (!provider_id || strcmp(provider_id, s_prov_desc.id) != 0) return NULL;
   clap_preset_discovery_provider_t *prov =
      (clap_preset_discovery_provider_t *)calloc(1, sizeof(*prov));
   if (!prov) return NULL;
   prov->desc = &s_prov_desc;
   prov->provider_data = (void *)indexer;
   prov->init = prov_init;
   prov->destroy = prov_destroy;
   prov->get_metadata = prov_get_metadata;
   prov->get_extension = prov_get_ext;
   return prov;
}

static uint32_t prov_factory_count(const clap_preset_discovery_factory_t *f) {
   (void)f; return 1;
}

static const clap_preset_discovery_provider_descriptor_t *prov_factory_desc(
      const clap_preset_discovery_factory_t *f, uint32_t i) {
   (void)f; return i == 0 ? &s_prov_desc : NULL;
}

static const clap_preset_discovery_factory_t s_discovery_factory = {
   prov_factory_count, prov_factory_desc, prov_create };

//////////////////////////  extension dispatch  ///////////////////////////

static const void *fx_get_extension(const clap_plugin_t *p, const char *id) {
   if (strcmp(id, CLAP_EXT_PARAMS) == 0) return &s_params;
   if (strcmp(id, CLAP_EXT_AUDIO_PORTS) == 0) return &s_audio_ports;
   if (strcmp(id, CLAP_EXT_STATE) == 0) return &s_state;
   if (strcmp(id, CLAP_EXT_LATENCY) == 0) return &s_latency;
   // v79: ONLY the gain plugin implements host-commanded preset load -
   // the sine is the negative-polarity control (supports_preset_load
   // must be false for it).
   if (!selfp(p)->is_sine && strcmp(id, CLAP_EXT_PRESET_LOAD) == 0)
      return &s_preset_load;
   return NULL;
}

static void fx_on_main_thread(const clap_plugin_t *p) { (void)p; }

//////////////////////////  factory + entry  //////////////////////////////

static const clap_plugin_descriptor_t s_desc_gain = {
   .clap_version = CLAP_VERSION_INIT,
   .id = "com.slooper.fixture-gain",
   .name = "Fixture Gain",
   .vendor = "slooper",
   .url = "https://github.com/actually-god/slooper",
   .manual_url = NULL,
   .support_url = NULL,
   .version = "1.0.0",
   .description = "Deterministic gain effect used by the slooper host pins",
   .features = (const char *[]){CLAP_PLUGIN_FEATURE_AUDIO_EFFECT, NULL},
};

static const clap_plugin_descriptor_t s_desc_sine = {
   .clap_version = CLAP_VERSION_INIT,
   .id = "com.slooper.fixture-sine",
   .name = "Fixture Sine",
   .vendor = "slooper",
   .url = "https://github.com/actually-god/slooper",
   .manual_url = NULL,
   .support_url = NULL,
   .version = "1.0.0",
   .description = "Deterministic sine instrument used by the slooper host pins",
   .features = (const char *[]){CLAP_PLUGIN_FEATURE_NOTE_EFFECT, NULL},
};

static const clap_plugin_t *fx_create(const clap_plugin_factory_t *factory,
                                      const clap_host_t *host, const char *id) {
   (void)factory;
   bool gain = strcmp(id, "com.slooper.fixture-gain") == 0;
   bool sine = strcmp(id, "com.slooper.fixture-sine") == 0;
   if (!gain && !sine) return NULL;

   fixture_plugin_t *f = (fixture_plugin_t *)calloc(1, sizeof(*f));
   if (!f) return NULL;
   f->gain = 1.0;
   f->amp = 1.0;
   f->is_sine = sine;
   for (int i = 0; i < SINE_VOICES; i++) f->voices[i].key = -1;

   static const clap_plugin_t s_vtable_gain = {
      .desc = &s_desc_gain,
      .init = fx_init,
      .destroy = fx_destroy,
      .activate = fx_activate,
      .deactivate = fx_deactivate,
      .start_processing = fx_start,
      .stop_processing = fx_stop,
      .reset = fx_reset,
      .process = fx_process,
      .get_extension = fx_get_extension,
      .on_main_thread = fx_on_main_thread,
   };
   static const clap_plugin_t s_vtable_sine = {
      .desc = &s_desc_sine,
      .init = fx_init,
      .destroy = fx_destroy,
      .activate = fx_activate,
      .deactivate = fx_deactivate,
      .start_processing = fx_start,
      .stop_processing = fx_stop,
      .reset = fx_reset,
      .process = fx_process,
      .get_extension = fx_get_extension,
      .on_main_thread = fx_on_main_thread,
   };
   f->plugin = gain ? s_vtable_gain : s_vtable_sine;
   f->plugin.plugin_data = f;
   f->host = host;
   return &f->plugin;
}

static uint32_t factory_count(const clap_plugin_factory_t *f) { (void)f; return 2; }
static const clap_plugin_descriptor_t *factory_desc(const clap_plugin_factory_t *f, uint32_t i) {
   (void)f;
   return i == 0 ? &s_desc_gain : (i == 1 ? &s_desc_sine : NULL);
}

static const clap_plugin_factory_t s_factory = {
   .get_plugin_count = factory_count,
   .get_plugin_descriptor = factory_desc,
   .create_plugin = fx_create,
};

static bool g_inited = false;
static bool entry_init(const char *plugin_path) {
   (void)plugin_path;
   g_inited = true;
   return true;
}
static void entry_deinit(void) { g_inited = false; }
static const void *entry_get_factory(const char *id) {
   if (strcmp(id, CLAP_PLUGIN_FACTORY_ID) == 0) return &s_factory;
   // v79: preset-discovery factory (primary + compat ids) so the host can
   // index locations and command loads through clap_plugin_preset_load.
   if (strcmp(id, CLAP_PRESET_DISCOVERY_FACTORY_ID) == 0 ||
       strcmp(id, CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT) == 0)
      return &s_discovery_factory;
   return NULL;
}

CLAP_EXPORT const clap_plugin_entry_t clap_entry = {
   .clap_version = CLAP_VERSION_INIT,
   .init = entry_init,
   .deinit = entry_deinit,
   .get_factory = entry_get_factory,
};
