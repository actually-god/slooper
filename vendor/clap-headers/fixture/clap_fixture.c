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
//
// This file is test infrastructure for src/claphost.rs; MIT like the host.

#include <math.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

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

#define PARAM_COUNT 2

static uint32_t pm_count(const clap_plugin_t *p) { (void)p; return PARAM_COUNT; }

static bool pm_get_info(const clap_plugin_t *p, uint32_t idx, clap_param_info_t *info) {
   (void)p;
   if (idx >= PARAM_COUNT) return false;
   memset(info, 0, sizeof(*info));
   if (idx == 0) {
      info->id = 1;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "fixgain");
      info->min_value = 0.0; info->max_value = 1.0; info->default_value = 1.0;
   } else {
      info->id = 2;
      snprintf(info->name, CLAP_NAME_SIZE, "%s", "fixamp");
      info->min_value = 0.0; info->max_value = 1.0; info->default_value = 1.0;
   }
   return true;
}

static bool pm_get_value(const clap_plugin_t *p, clap_id id, double *out) {
   fixture_plugin_t *f = selfp(p);
   if (id == 1) { *out = f->gain; return true; }
   if (id == 2) { *out = f->amp; return true; }
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

//////////////////////////  extension dispatch  ///////////////////////////

static const void *fx_get_extension(const clap_plugin_t *p, const char *id) {
   (void)p;
   if (strcmp(id, CLAP_EXT_PARAMS) == 0) return &s_params;
   if (strcmp(id, CLAP_EXT_AUDIO_PORTS) == 0) return &s_audio_ports;
   if (strcmp(id, CLAP_EXT_STATE) == 0) return &s_state;
   if (strcmp(id, CLAP_EXT_LATENCY) == 0) return &s_latency;
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
   return NULL;
}

CLAP_EXPORT const clap_plugin_entry_t clap_entry = {
   .clap_version = CLAP_VERSION_INIT,
   .init = entry_init,
   .deinit = entry_deinit,
   .get_factory = entry_get_factory,
};
