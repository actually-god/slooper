/* w6_vst2_fixture.c — WINDOWS-PORT W6 test fixture: a minimal but VALID
 * VST2 effect, compiled to BOTH:
 *   tests/fixtures/w6/w6-vst2.dll   (llvm-mingw, -static; the Windows pins
 *                                   scan+probe it as a native VST2 plugin)
 *   tests/fixtures/w6/w6-vst2.so    (host cc; the Linux pins probe the SAME
 *                                   semantics and prove the probe path)
 * Rebuild both with:  scripts/porting/w6_build_fixtures.sh
 *
 * It is only as capable as the slooper scan probe requires (vendored vst2
 * PluginLoader): `VSTPluginMain` returns a static AEffect whose dispatcher
 * answers product/vendor/API version. The opcode numbers below are the
 * discriminants of the vendored crate's `plugin::OpCode` (NOT the VST2.4
 * SDK eff* values — the probe talks the crate's dialect; verified by
 * `cargo test --lib w6_linux_fixture_vst2_probes_clean`, which must read
 * back the exact name below).
 *
 * numInputs=1/numOutputs=1, flags=0 => the entry is an EFFECT (instrument
 * rule: effFlagsSynth || inputs==0). `scan` must never drop it or mark it
 * errored.
 */
#include <stdint.h>
#include <string.h>

#if defined(_WIN32)
#define W6EXPORT __declspec(dllexport)
#else
#define W6EXPORT __attribute__((visibility("default")))
#endif

typedef struct W6AEffect W6AEffect;
typedef long (*W6Dispatch)(W6AEffect *e, int opcode, int index, long value,
                           void *ptr, float opt);
typedef void (*W6SetParam)(W6AEffect *e, int index, float value);
typedef float (*W6GetParam)(W6AEffect *e, int index);
typedef void (*W6Process)(W6AEffect *e, float *const *ins, float *const *outs,
                          int frames);
typedef void (*W6ProcessF64)(W6AEffect *e, double *const *ins,
                             double *const *outs, int frames);

/* Mirrors vst2::api::AEffect (repr C) — field-for-field. */
struct W6AEffect {
    int32_t magic;
    W6Dispatch dispatcher;
    W6Process process;
    W6SetParam setParameter;
    W6GetParam getParameter;
    int32_t numPrograms, numParams, numInputs, numOutputs, flags;
    intptr_t reserved1, reserved2;
    int32_t initialDelay, realQualities, offQualities;
    float ioRatio;
    void *object, *user;
    int32_t uniqueId, version;
    W6Process processReplacing;
    W6ProcessF64 processReplacingF64;
    unsigned char future[56];
};

/* vendored vst2::plugin::OpCode discriminants (see header note) */
#define W6OP_GetCategory 35
#define W6OP_GetVendorName 47
#define W6OP_GetProductName 48
#define W6OP_GetApiVersion 58

/* v60: factory-program dialect — crate OpCode ChangePreset=2,
 * GetCurrentPresetNum=3, GetCurrentPresetName=5 (these three coincide
 * with the VST2.4 eff* numbers because the enum keeps the SDK's leading
 * order). A program index >= numPrograms answers -1 for ChangePreset:
 * the v60 pins require the host to surface that as a refused select. */
#define W6OP_ChangePreset 2
#define W6OP_GetCurrentPresetNum 3
#define W6OP_GetCurrentPresetName 5
#define W6_NUM_PROGRAMS 3
static const char *W6_PROGRAMS[W6_NUM_PROGRAMS] = {"Init", "Fat", "Thin"};
static int g_program = 0;

#define W6_NAME "W6 VST Fixture"
#define W6_VENDOR "slooper W6"

static void w6_copy64(char *dst, const char *src) {
    if (!dst)
        return;
    strncpy(dst, src, 63);
    dst[63] = 0;
}

/* v103 (card t_62c5a14e): MIDI receive observer. effProcessEvents carries
 * the crate-enum ordinal (25 — NOT the SDK's 9; same dialect note as the
 * program block above; verified against vendor/vst2 plugin.rs). We record
 * per-message: RPN slot selects (CC101/100), RPN-0 data writes (CC6/CC38
 * while slot 0/0), pitch bend, and poly aftertouch (v104). Read back from
 * host pins via private opcodes 9000..9006 (never sent by real hosts —
 * default 0 otherwise). */
#define W6OP_ProcessEvents 25
#define W6OP_ProbeRpnWrites 9000 /* CC6 lands while RPN slot 0/0 active */
#define W6OP_ProbeRpnMsb 9001    /* last CC6 value (-1 = never)           */
#define W6OP_ProbeRpnLsb 9002    /* last CC38 value (-1 = never)          */
#define W6OP_ProbeBend 9003      /* last pitch bend 14-bit + 1 (0 = none) */
/* v104 (card t_0ab25d9a): poly aftertouch (0xA0) observer. */
#define W6OP_ProbePoly 9004      /* count of 0xA0 messages seen           */
#define W6OP_ProbePolyNote 9005  /* (channel << 8) | note (-1 = never)    */
#define W6OP_ProbePolyPress 9006 /* last poly-AT pressure (-1 = never)    */

static int g_rpn_sel_msb = -1; /* last CC101 value            */
static int g_rpn_active = 0;   /* slot 0/0 currently selected */
static int g_rpn_writes = 0;
static int g_rpn_msb = -1;
static int g_rpn_lsb = -1;
static int g_bend = 0;
static int g_poly_msgs = 0; /* v104: 0xA0 count                */
static int g_poly_ch = -1;  /* v104: last poly-AT channel      */
static int g_poly_note = -1; /* v104: last poly-AT note        */
static int g_poly_press = -1; /* v104: last poly-AT pressure   */

/* Mirror of vst2::api::MidiEvent's leading fields (repr C): six i32s of
 * header + note fields, then the 3 midi bytes. */
typedef struct {
    int32_t type, byte_size, delta_frames, flags, note_length, note_offset;
    uint8_t midi[3];
} w6_midi_event;

typedef struct {
    int32_t num_events;
    intptr_t reserved; /* auto-pads to 8-align like the crate's EvBatch */
    void *events[];
} w6_events;

static void w6_note_midi(const uint8_t *m) {
    int status = m[0] & 0xF0;
    if (status == 0xE0) {
        g_bend = ((m[2] << 7) | m[1]) + 1; /* +1: 0 means "never" */
        return;
    }
    if (status == 0xA0) { /* v104: poly aftertouch passthrough observer */
        g_poly_msgs++;
        g_poly_ch = m[0] & 0x0F;
        g_poly_note = m[1];
        g_poly_press = m[2];
        return;
    }
    if (status != 0xB0)
        return;
    int cc = m[1], v = m[2];
    if (cc == 101)
        g_rpn_sel_msb = v;
    else if (cc == 100)
        g_rpn_active = (g_rpn_sel_msb == 0 && v == 0);
    else if (cc == 6 && g_rpn_active) {
        g_rpn_msb = v;
        g_rpn_writes++;
    } else if (cc == 38 && g_rpn_active)
        g_rpn_lsb = v;
}

static void w6_process_events(void *ptr) {
    w6_events *ev = (w6_events *)ptr;
    if (!ev)
        return;
    int n = ev->num_events;
    if (n < 0)
        n = 0;
    if (n > 4096)
        n = 4096;
    for (int i = 0; i < n; i++) {
        w6_midi_event *m = (w6_midi_event *)ev->events[i];
        /* EventType::Midi = 1 (vendored api enum). */
        if (m && m->type == 1)
            w6_note_midi(m->midi);
    }
}

static long w6_dispatch(W6AEffect *e, int opcode, int index, long value,
                        void *ptr, float opt) {
    (void)e;
    (void)index;
    (void)value;
    (void)opt;
    switch (opcode) {
    case W6OP_GetCategory:
        return 1; /* plugin::Category::Effect */
    case W6OP_GetVendorName:
        w6_copy64((char *)ptr, W6_VENDOR);
        return 1;
    case W6OP_GetProductName:
        w6_copy64((char *)ptr, W6_NAME);
        return 1;
    case W6OP_GetApiVersion:
        return 2400;
    case W6OP_ChangePreset:
        if (value < 0 || value >= W6_NUM_PROGRAMS)
            return -1;
        g_program = (int)value;
        return g_program;
    case W6OP_GetCurrentPresetNum:
        return g_program;
    case W6OP_GetCurrentPresetName:
        if (index < 0 || index >= W6_NUM_PROGRAMS)
            return 0;
        w6_copy64((char *)ptr, W6_PROGRAMS[index]);
        return 1;
    case W6OP_ProcessEvents:
        w6_process_events(ptr);
        return 1;
    case W6OP_ProbeRpnWrites:
        return g_rpn_writes;
    case W6OP_ProbeRpnMsb:
        return g_rpn_msb;
    case W6OP_ProbeRpnLsb:
        return g_rpn_lsb;
    case W6OP_ProbeBend:
        return g_bend;
    case W6OP_ProbePoly:
        return g_poly_msgs;
    case W6OP_ProbePolyNote:
        return g_poly_note < 0 ? -1 : ((g_poly_ch << 8) | g_poly_note);
    case W6OP_ProbePolyPress:
        return g_poly_press;
    default:
        return 0;
    }
}

static void w6_process(W6AEffect *e, float *const *i, float *const *o,
                       int f) {
    (void)e;
    (void)i;
    (void)o;
    (void)f;
}
static void w6_process64(W6AEffect *e, double *const *i, double *const *o,
                         int f) {
    (void)e;
    (void)i;
    (void)o;
    (void)f;
}
static void w6_setparam(W6AEffect *e, int i, float v) {
    (void)e;
    (void)i;
    (void)v;
}
static float w6_getparam(W6AEffect *e, int i) {
    (void)e;
    (void)i;
    return 0.0f;
}

static W6AEffect g_fx;

W6EXPORT void *VSTPluginMain(void *host_callback) {
    (void)host_callback;
    memset(&g_fx, 0, sizeof g_fx);
    /* v60: every VSTPluginMain call hands the host a FRESH plugin state,
     * like a real effect allocating a new AEffect — the program counter
     * must not carry from a previous instance of the same file (the
     * v60_vst2_open_resets_program pin asserts this contract). */
    g_program = 0;
    /* v103: same fresh-instance contract for the MIDI observer statics. */
    g_rpn_sel_msb = -1;
    g_rpn_active = 0;
    g_rpn_writes = 0;
    g_rpn_msb = -1;
    g_rpn_lsb = -1;
    g_bend = 0;
    /* v104: poly-AT observer statics ride the same fresh-instance rule. */
    g_poly_msgs = 0;
    g_poly_ch = -1;
    g_poly_note = -1;
    g_poly_press = -1;
    g_fx.magic = 0x56737450; /* 'VstP' */
    g_fx.dispatcher = w6_dispatch;
    g_fx.process = w6_process;
    g_fx.setParameter = w6_setparam;
    g_fx.getParameter = w6_getparam;
    g_fx.numPrograms = W6_NUM_PROGRAMS;
    g_fx.numParams = 0;
    g_fx.numInputs = 1;
    g_fx.numOutputs = 1;
    g_fx.flags = 1 << 4; /* CAN_REPLACING only: no editor, no synth */
    g_fx.uniqueId = 0x57364631;
    g_fx.version = 1;
    g_fx.processReplacing = w6_process;
    g_fx.processReplacingF64 = w6_process64;
    return &g_fx;
}
