// Dev probe for the fixture: drives it through the raw CLAP ABI exactly the
// way src/claphost.rs will, printing deterministic expectations.
#include <math.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>

#include <clap/clap.h>

static clap_event_note_t g_note; // the single staged event
static uint32_t in_size(const clap_input_events_t *l) { return (uint32_t)(intptr_t)l->ctx; }
static const clap_event_header_t *in_get(const clap_input_events_t *l, uint32_t i) {
   (void)l;
   return i == 0 ? (const clap_event_header_t *)&g_note : NULL;
}

int main(int argc, char **argv) {
   if (argc < 2) { fprintf(stderr, "usage: probe fixture.clap\n"); return 2; }
   void *lib = dlopen(argv[1], RTLD_NOW | RTLD_LOCAL);
   if (!lib) { fprintf(stderr, "dlopen: %s\n", dlerror()); return 1; }
   const clap_plugin_entry_t *e = dlsym(lib, "clap_entry");
   if (!e) { fprintf(stderr, "no clap_entry\n"); return 1; }
   printf("entry version %u.%u.%u\n", e->clap_version.major, e->clap_version.minor,
          e->clap_version.revision);
   if (!e->init(argv[1])) { fprintf(stderr, "init failed\n"); return 1; }
   const clap_plugin_factory_t *f = e->get_factory(CLAP_PLUGIN_FACTORY_ID);
   if (!f) { fprintf(stderr, "no factory\n"); return 1; }
   uint32_t count = f->get_plugin_count(f);
   printf("plugin count: %u\n", count);
   for (uint32_t i = 0; i < count; i++) {
      const clap_plugin_descriptor_t *d = f->get_plugin_descriptor(f, i);
      printf("  [%u] %s / %s features:", i, d->id, d->name);
      for (const char *const *ft = d->features; ft && *ft; ft++) printf(" %s", *ft);
      printf("\n");
   }

   // --- instantiate the sine, feed a note, measure amplitude ---
   const clap_plugin_t *p = f->create_plugin(f, NULL, "com.slooper.fixture-sine");
   if (!p) { fprintf(stderr, "create failed\n"); return 1; }
   if (!p->init(p)) { fprintf(stderr, "plugin init failed\n"); return 1; }
   const clap_plugin_params_t *pm = p->get_extension(p, CLAP_EXT_PARAMS);
   printf("params count: %u\n", pm ? pm->count(p) : 0);
   clap_param_info_t pi;
   pm->get_info(p, 0, &pi);
   printf("param0: id=%u name=%s range=[%g,%g] def=%g\n", pi.id, pi.name, pi.min_value,
          pi.max_value, pi.default_value);
   if (!p->activate(p, 48000.0, 1, 512)) { fprintf(stderr, "activate failed\n"); return 1; }
   p->start_processing(p);

   clap_event_note_t note_on;
   memset(&note_on, 0, sizeof(note_on));
   note_on.header.size = sizeof(note_on);
   note_on.header.type = CLAP_EVENT_NOTE_ON;
   note_on.header.time = 0;
   note_on.note_id = -1;
   note_on.port_index = -1;
   note_on.channel = 0;
   note_on.key = 69;
   note_on.velocity = 1.0;

   clap_input_events_t in;
   in.ctx = (void *)(intptr_t)1; // one event
   in.size = in_size;
   in.get = in_get;
   g_note = note_on;

   clap_output_events_t out;
   out.ctx = NULL;
   out.try_push = NULL;

   float l[512], r[512];
   float *lp[1] = {l}, *rp[1] = {r};
   clap_audio_buffer_t ab_in = {0};
   clap_audio_buffer_t ab_out = {0};
   (void)ab_in; (void)lp;
   ab_out.data32 = rp;
   ab_out.channel_count = 1;
   float *chan[2] = {l, r};
   ab_out.data32 = chan;
   ab_out.channel_count = 2;

   clap_process_t pr;
   memset(&pr, 0, sizeof(pr));
   pr.steady_time = 0;
   pr.frames_count = 512;
   pr.audio_inputs = NULL;
   pr.audio_outputs = &ab_out;
   pr.audio_inputs_count = 0;
   pr.audio_outputs_count = 1;
   pr.in_events = &in;
   pr.out_events = &out;

   int st = p->process(p, &pr);
   double peak = 0.0;
   for (int i = 0; i < 512; i++) { double a = fabs(l[i]); if (a > peak) peak = a; }
   printf("process status=%d peak=%.4f (expect ~0.5)\n", st, peak);
   // zero-cross rate ~ period estimate
   int zc = 0;
   for (int i = 1; i < 512; i++) if ((l[i - 1] < 0) != (l[i] < 0)) zc++;
   printf("zero crossings %d (expect ~%.0f for 440Hz@48k)\n", zc, 440.0 * 2.0 * 512.0 / 48000.0);

   // state round trip
   const clap_plugin_state_t *stx = p->get_extension(p, CLAP_EXT_STATE);
   char sbuf[128];
   size_t slen = 0;
   int64_t sw(const struct clap_ostream *s, const void *b, uint64_t n) {
      (void)s; memcpy(sbuf + slen, b, n); slen += n; return (int64_t)n;
   }
   clap_ostream_t os = { .ctx = NULL, .write = sw };
   bool ok = stx->save(p, &os);
   sbuf[slen] = 0;
   printf("state save ok=%d len=%zu content=%s", ok, slen, sbuf);

   p->stop_processing(p);
   p->deactivate(p);
   p->destroy(p);
   e->deinit();
   return 0;
}
