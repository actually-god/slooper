/* v33 MIDI oracle: a minimal VST2 instrument whose ONLY job is to report,
 * with zero doubt, what the HOST actually delivered. Counters are dumped to
 * the file named by $ORACLE_STATS after every process() call as JSON:
 *   {"ev_calls":..,"ev_total":..,"note_on":..,"note_off":..,"cc":..,
 *    "other":..,"process_calls":..,"frames":..,"voices":..,"delta_sum":..,
 *    "max_batch":..,"param_sets":..,"opens":..,"closes":..}
 * Audio out is a DC level of voices/16 so the host's own meters measure
 * polyphony. AEvent layout mirrors the vendored vst-rs api::AEffect EXACTLY
 * (it differs from Steinberg's header: no audioMaster field, magic first;
 * api::Events carries no dispatch pointer).
 * Build: cc -shared -fPIC -O1 -o oracle.so oracle.c
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

typedef struct AEffect AEffect;
typedef intptr_t (*dispatch_t)(AEffect *e, int opcode, int index,
                               intptr_t value, void *ptr, float opt);

/* vst-rs api::AEffect mirror */
struct AEffect {
    int32_t magic;                    /* 0x56737450 */
    dispatch_t dispatcher;
    void *_process;
    void (*setParameter)(AEffect *e, int index, float value);
    float (*getParameter)(AEffect *e, int index);
    int32_t numPrograms, numParams, numInputs, numOutputs, flags;
    intptr_t reserved1, reserved2;
    int32_t initialDelay, _rq, _oq;
    float _io;
    void *object, *user;
    int32_t uniqueId, version;
    void (*processReplacing)(AEffect *e, float **ins, float **outs, int n);
    void *_unused2;
    uint8_t future[56];
};

/* vst-rs MidiEvent mirror */
typedef struct {
    int32_t type, byte_size, delta_frames, flags;
    int32_t note_length, note_offset;
    char midi[4];
    char detune, offvel, r1, r2;
} MidiEventC;

/* vst-rs Events mirror (NO dispatch field!) */
typedef struct {
    int32_t num_events;
    intptr_t _reserved;
    void *events[2];
} EventsC;

static struct {
    long ev_calls, ev_total, note_on, note_off, cc, other;
    long process_calls, frames;
    long voices;                 /* currently-held note ids (any channel) */
    long delta_sum, max_batch, param_sets, opens, closes;
    char held[16 * 128];
    char stats_path[512];
} S;

static void dump(void) {
    if (!S.stats_path[0]) return;
    FILE *f = fopen(S.stats_path, "w");
    if (!f) return;
    fprintf(f,
        "{\"ev_calls\":%ld,\"ev_total\":%ld,\"note_on\":%ld,\"note_off\":%ld,"
        "\"cc\":%ld,\"other\":%ld,\"process_calls\":%ld,\"frames\":%ld,"
        "\"voices\":%ld,\"delta_sum\":%ld,\"max_batch\":%ld,\"param_sets\":%ld,"
        "\"opens\":%ld,\"closes\":%ld}",
        S.ev_calls, S.ev_total, S.note_on, S.note_off, S.cc, S.other,
        S.process_calls, S.frames, S.voices, S.delta_sum, S.max_batch,
        S.param_sets, S.opens, S.closes);
    fclose(f);
}

static intptr_t dispatch(AEffect *e, int op, int index, intptr_t value,
                         void *ptr, float opt) {
    switch (op) {
        case 0: S.opens++; return 0;             /* effOpen */
        case 1: S.closes++; dump(); return 0;    /* effClose */
        case 25: {                               /* effProcessEvents */
            EventsC *ev = (EventsC *)ptr;
            int n = ev->num_events;
            if (n > 0) {
                S.ev_calls++;
                if (n > S.max_batch) S.max_batch = n;
                for (int i = 0; i < n; i++) {
                    MidiEventC *m = (MidiEventC *)ev->events[i];
                    if (!m || m->type != 1) { S.other++; continue; }
                    S.ev_total++;
                    S.delta_sum += m->delta_frames;
                    unsigned char st = (unsigned char)m->midi[0];
                    int hi = st & 0xF0, ch = st & 0x0F;
                    int note = (unsigned char)m->midi[1];
                    int vel = (unsigned char)m->midi[2];
                    int slot = ch * 128 + (note & 127);
                    if (hi == 0x90 && vel > 0) {
                        S.note_on++;
                        if (!S.held[slot]) { S.held[slot] = 1; S.voices++; }
                    } else if (hi == 0x80 || (hi == 0x90 && vel == 0)) {
                        S.note_off++;
                        if (S.held[slot]) { S.held[slot] = 0; S.voices--; }
                    } else if (hi == 0xB0) {
                        S.cc++;
                        /* Real-world: CC 120-123 release held notes on
                         * the channel (all sounds off / all notes off). */
                        if (note >= 120 && note <= 123) {
                            for (int k = 0; k < 128; k++) {
                                int s2 = ch * 128 + k;
                                if (S.held[s2]) { S.held[s2] = 0; S.voices--; }
                            }
                        }
                    }
                    else S.other++;
                }
            }
            return 0;
        }
        default:
            /* Any string-fetch query (names/labels/displays): fill the
             * buffer so the host never reads garbage. */
            if (ptr && (op == 4 || op == 5 || op == 6 || op == 7 || op == 8 ||
                        op == 29 || (op >= 45 && op <= 50))) {
                memset(ptr, 0, 64);
                strcpy((char *)ptr, "SlooperOracle");
                return 1;
            }
            return 0;
    }
}

static void set_parameter(AEffect *e, int index, float value) {
    (void)e; (void)index; (void)value;
    S.param_sets++;
}

static float get_parameter(AEffect *e, int index) {
    (void)e;
    switch (index) {
        case 0: return (float)S.ev_total;
        case 1: return (float)S.note_on;
        case 2: return (float)S.note_off;
        case 3: return (float)S.process_calls;
        case 4: return (float)S.voices;
        default: return 0.0f;
    }
}

static void process(AEffect *e, float **ins, float **outs, int n) {
    (void)e; (void)ins;
    float v = (float)S.voices / 16.0f;
    S.process_calls++;
    S.frames += n;
    for (int i = 0; i < n; i++) {
        if (outs[0]) outs[0][i] = v;
        if (outs[1]) outs[1][i] = v;
    }
    dump();
}

/* vst-rs process_double_replacing signature differs; never called. */
static void process64(AEffect *e, void **ins, void **outs, int n) {
    (void)e; (void)ins; (void)outs; (void)n;
}

static AEffect EFFECT;

static AEffect *instantiate(void *audioMaster) {
    const char *p = getenv("ORACLE_STATS");
    if (p) { strncpy(S.stats_path, p, sizeof(S.stats_path) - 1); }
    memset(&EFFECT, 0, sizeof(EFFECT));
    EFFECT.magic = 0x56737450;
    EFFECT.dispatcher = dispatch;
    EFFECT._process = (void *)process;      /* legacy accumulate mode: unused */
    EFFECT.setParameter = set_parameter;
    EFFECT.getParameter = get_parameter;
    EFFECT.numPrograms = 1;
    EFFECT.numParams = 9;
    EFFECT.numInputs = 1;
    EFFECT.numOutputs = 2;
    /* effFlagsHasEditor|effFlagsCanReplacing|effFlagsPrograms|effFlagsSynth */
    EFFECT.flags = 1 | 32 | 8 | 256;
    EFFECT.uniqueId = 0x534F7231;           /* 'SOr1' */
    EFFECT.version = 3300;
    EFFECT.processReplacing = process;
    EFFECT._unused2 = (void *)process64;
    dump();
    (void)audioMaster;
    return &EFFECT;
}

__attribute__((visibility("default"))) AEffect *VSTPluginMain(void *audioMaster) {
    return instantiate(audioMaster);
}
__attribute__((visibility("default"))) AEffect *main(void *audioMaster) {
    return instantiate(audioMaster);
}
