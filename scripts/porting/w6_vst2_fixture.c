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

#define W6_NAME "W6 VST Fixture"
#define W6_VENDOR "slooper W6"

static void w6_copy64(char *dst, const char *src) {
    if (!dst)
        return;
    strncpy(dst, src, 63);
    dst[63] = 0;
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
    g_fx.magic = 0x56737450; /* 'VstP' */
    g_fx.dispatcher = w6_dispatch;
    g_fx.process = w6_process;
    g_fx.setParameter = w6_setparam;
    g_fx.getParameter = w6_getparam;
    g_fx.numPrograms = 1;
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
