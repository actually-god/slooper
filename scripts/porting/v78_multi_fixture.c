/* v78_multi_fixture.c — a dependency-free VST3 plugin with TWO units,
 * each carrying its OWN program list + kIsProgramChange parameter.
 *
 * Purpose: the multi-part class v66 could only refuse (TS-015 / card
 * t_f4cd4c9e). slooper's runtime relay can only move a parameter BY ID,
 * and vst3-host 0.10's get_parameters() report hides ParameterInfo.unitId,
 * so the host must resolve "which cell drives unit 0" by probing
 * select_program + diffing candidate values. This fixture gives that
 * resolution a REAL two-unit target, and — critically — exposes BOTH
 * program variables in getState so a pin can prove WHICH unit's cell moved:
 * selecting a drum kit must change prog0 and leave prog1 (a foreign unit's
 * cell) untouched. A host that wires the wrong cell fails loudly.
 *
 * ABI: same raw-C COM layout as v66_vst3_fixture.c (byte-for-byte the
 * `vst3` 0.3.0 Rust bindings vst3-host compiles against, per-platform
 * IID bytes). Keep the two files' shared scaffolding in sync.
 *
 * Surface: one class "V78 Multi" (Audio Module Class). Params:
 *   id 100  "level"         continuous,        unit 0
 *   id 1000 "kit"           kIsProgramChange|kIsList|kIsHidden, unit 0 (3 programs)
 *   id 1001 "patch"         kIsProgramChange|kIsList|kIsHidden, unit 1 (2 programs)
 * Units: 0 "drums" (program list 7) / 1 "synth" parent 0 (program list 8).
 * Lists: 7 "drum kits" = kit-a, kit-b, kit-c; 8 "synth patches" = warm, bright.
 * getState/setState carry (int32 prog0, int32 prog1, double level) (16 bytes).
 * Program state is PER INSTANCE. Both apply routes (controller
 * setParamNormalized and the process-cycle inputParameterChanges pull)
 * converge on the same variable per unit. process() outputs silence.
 *
 * Build: scripts/porting/v78_build_fixtures.sh (host cc + llvm-mingw gcc,
 * binaries COMMITTED under tests/fixtures/v78 like the v66 flow).
 */
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

#if defined(_WIN32)
#define FX_API __declspec(dllexport)
#else
#define FX_API __attribute__((visibility("default")))
#endif

typedef int32_t tresult;
#define kResultOk ((tresult)0)
#define kNoInterface ((tresult)-1)
#define kResultFalse ((tresult)-2)
#define kNotImplemented ((tresult)-3)

typedef uint32_t ParamID;
typedef double ParamValue;
typedef int32_t UnitID;
typedef int32_t ProgramListID;
typedef int32_t MediaType;
typedef int32_t BusDirection;
typedef int32_t BusType;
typedef uint64_t SpeakerArrangement;
typedef uint16_t TChar;
typedef uint8_t TBool;
#define kStringSize 128

/* IIDs per PLATFORM layout — see the v66 fixture header: the `vst3` 0.3.0
 * bindings' support::uid() is OS-dependent (Windows = classic mixed-endian
 * GUID bytes; non-Windows = big-endian per 32-bit group). */
#if defined(_WIN32)
static const uint8_t IID_FUNKNOWN[16]          = {0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xC0,0x00,0x00,0x00,0x00,0x00,0x00,0x46};
static const uint8_t IID_IPLUGINBASE[16]       = {0xDB,0x8D,0x88,0x22,0x6E,0x15,0xAE,0x45,0x83,0x58,0xB3,0x48,0x08,0x19,0x06,0x25};
static const uint8_t IID_ICOMPONENT[16]        = {0x31,0xFF,0x31,0xE8,0xD5,0xF2,0x01,0x43,0x92,0x8E,0xBB,0xEE,0x25,0x69,0x78,0x02};
static const uint8_t IID_IAUDIOPROCESSOR[16]   = {0x99,0x3F,0x04,0x42,0xDA,0xB7,0x3C,0x45,0xA5,0x69,0xE7,0x9D,0x9A,0xAE,0xC3,0x3D};
static const uint8_t IID_IEDITCONTROLLER[16]   = {0xE3,0xBB,0xD7,0xDC,0x42,0x77,0x8D,0x44,0xA8,0x74,0xAA,0xCC,0x97,0x9C,0x75,0x9E};
static const uint8_t IID_IUNITINFO[16]         = {0xB5,0xD6,0x4B,0x3D,0x3A,0x91,0xD2,0x4F,0xA8,0x86,0xE7,0x68,0xA5,0xEB,0x92,0xC1};
static const uint8_t IID_IPLUGINFACTORY[16]    = {0x1C,0x81,0x4D,0x7A,0x11,0x52,0x1F,0x4A,0xAE,0xD9,0xD2,0xEE,0x0B,0x43,0xBF,0x9F};
#else
static const uint8_t IID_FUNKNOWN[16]          = {0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0xC0,0x00,0x00,0x00,0x00,0x00,0x00,0x46};
static const uint8_t IID_IPLUGINBASE[16]       = {0x22,0x88,0x8D,0xDB,0x15,0x6E,0x45,0xAE,0x83,0x58,0xB3,0x48,0x08,0x19,0x06,0x25};
static const uint8_t IID_ICOMPONENT[16]        = {0xE8,0x31,0xFF,0x31,0xF2,0xD5,0x43,0x01,0x92,0x8E,0xBB,0xEE,0x25,0x69,0x78,0x02};
static const uint8_t IID_IAUDIOPROCESSOR[16]   = {0x42,0x04,0x3F,0x99,0xB7,0xDA,0x45,0x3C,0xA5,0x69,0xE7,0x9D,0x9A,0xAE,0xC3,0x3D};
static const uint8_t IID_IEDITCONTROLLER[16]   = {0xDC,0xD7,0xBB,0xE3,0x77,0x42,0x44,0x8D,0xA8,0x74,0xAA,0xCC,0x97,0x9C,0x75,0x9E};
static const uint8_t IID_IUNITINFO[16]         = {0x3D,0x4B,0xD6,0xB5,0x91,0x3A,0x4F,0xD2,0xA8,0x86,0xE7,0x68,0xA5,0xEB,0x92,0xC1};
static const uint8_t IID_IPLUGINFACTORY[16]    = {0x7A,0x4D,0x81,0x1C,0x52,0x11,0x4A,0x1F,0xAE,0xD9,0xD2,0xEE,0x0B,0x43,0xBF,0x9F};
#endif

/* ParameterInfo flags (ivsteditcontroller.h) — kIsProgramChange is 1<<15. */
#define kCanAutomate 1
#define kIsList 8
#define kIsHidden 16
#define kIsProgramChange 32768
#define kDefaultActive 1

#define PID_LEVEL 100u
#define PID_KIT 1000u
#define PID_PATCH 1001u
#define LIST_DRUMS 7
#define LIST_SYNTH 8
#define N_DRUMS 3
#define N_SYNTH 2
static const char* kDrumNames[N_DRUMS] = {"kit-a", "kit-b", "kit-c"};
static const char* kSynthNames[N_SYNTH] = {"warm", "bright"};
/* class uid: fixed 16 bytes, distinct from the v66 fixture. */
static const uint8_t CLASS_UID[16] = {'V','7','8','m','u','l','t','1','0','1','2','3','4','5','6','7'};

/* ---- host-side interfaces we CALL: same queue/stream shapes as v66 ---- */

/* ---- shared data structs (byte-for-byte the bindings' repr(C)) ---- */
typedef struct { uint32_t id; TChar title[kStringSize]; TChar shortTitle[kStringSize]; TChar units[kStringSize];
                 int32_t stepCount; ParamValue defaultNormalizedValue; UnitID unitId; int32_t flags; } ParameterInfo;
typedef struct { UnitID id; UnitID parentUnitId; TChar name[kStringSize]; ProgramListID programListId; } UnitInfo;
typedef struct { ProgramListID id; TChar name[kStringSize]; int32_t programCount; } ProgramListInfo;
typedef struct { MediaType mediaType; BusDirection direction; int32_t channelCount; TChar name[kStringSize];
                 BusType busType; uint32_t flags; } BusInfo;
typedef struct { int32_t processMode; int32_t symbolicSampleSize; int32_t numSamples; int32_t numInputs;
                 int32_t numOutputs; void* inputs; void* outputs; void* inputParameterChanges;
                 void* outputParameterChanges; void* inputEvents; void* outputEvents; void* processContext; } ProcessData;
typedef struct { int32_t processMode; int32_t symbolicSampleSize; int32_t maxSamplesPerBlock; double sampleRate; } ProcessSetup;
typedef struct { int32_t numChannels; uint64_t silenceFlags; void* channelBuffers; } AudioBusBuffers;
typedef char char8;
typedef struct { char8 vendor[64]; char8 url[256]; char8 email[128]; int32_t flags; } PFactoryInfo;
typedef struct { uint8_t cid[16]; int32_t cardinality; char8 category[32]; char8 name[64]; } PClassInfo;

/* ---- the instance ---- */
typedef struct Fixture Fixture;
typedef struct { const void** vt; Fixture* self; } Slot;

struct Fixture {
    int refs;
    int kit;          /* unit 0 program 0..N_DRUMS-1 (controller AND processor converge here) */
    int patch;        /* unit 1 program 0..N_SYNTH-1  (must NOT move when unit 0 changes) */
    double level;
    double sample_rate;
    Slot comp;        /* FUnknown / IPluginBase / IComponent */
    Slot ap;          /* IAudioProcessor */
    Slot ec;          /* IEditController */
    Slot ui;          /* IUnitInfo */
};

#define FX(self0) (((Slot*)(self0))->self)

static void put_name(TChar out[kStringSize], const char* ascii) {
    int i = 0;
    for (; ascii[i]; i++) out[i] = (TChar)(unsigned char)ascii[i];
    out[i] = 0;
}

static int clamp_kit(double norm) {
    double s = norm * (double)(N_DRUMS - 1);
    int p = (int)(s + (s >= 0 ? 0.5 : -0.5));
    if (p < 0) p = 0;
    if (p > N_DRUMS - 1) p = N_DRUMS - 1;
    return p;
}
static int clamp_patch(double norm) {
    double s = norm * (double)(N_SYNTH - 1);
    int p = (int)(s + (s >= 0 ? 0.5 : -0.5));
    if (p < 0) p = 0;
    if (p > N_SYNTH - 1) p = N_SYNTH - 1;
    return p;
}

/* ---- FUnknown family (shared by every slot) ---- */
static tresult unk_qi(void* self0, const uint8_t iid[16], void** obj);
static uint32_t unk_addref(void* self0);
static uint32_t unk_release(void* self0);

/* ---- IComponent (+ IPluginBase prefix) ---- */
static tresult c_initialize(void* s, void* ctx) { (void)s; (void)ctx; return kResultOk; }
static tresult c_terminate(void* s) { (void)s; return kResultOk; }
static tresult c_getControllerClassId(void* s, uint8_t* cid) { (void)s; (void)cid; return kNotImplemented; }
static tresult c_setIoMode(void* s, int32_t m) { (void)s; (void)m; return kNotImplemented; }
static int32_t c_getBusCount(void* s, MediaType media, BusDirection dir) {
    (void)s;
    if (media == 0) return 1;                    /* audio: 1 in, 1 out */
    if (media == 1) return dir == 0 ? 1 : 0;     /* event: 1 in, 0 out */
    return 0;
}
static tresult c_getBusInfo(void* s, MediaType media, BusDirection dir, int32_t index, BusInfo* bus) {
    (void)s;
    if (!bus || index != 0) return kResultFalse;
    memset(bus, 0, sizeof(*bus));
    bus->mediaType = media;
    bus->direction = dir;
    bus->busType = 0; /* Main */
    bus->flags = kDefaultActive;
    if (media == 0) { bus->channelCount = 2; put_name(bus->name, dir == 0 ? "In" : "Out"); }
    else { bus->channelCount = 1; put_name(bus->name, "Events"); }
    return kResultOk;
}
static tresult c_getRoutingInfo(void* s, void* a, void* b) { (void)s; (void)a; (void)b; return kNotImplemented; }
static tresult c_activateBus(void* s, MediaType media, BusDirection dir, int32_t index, TBool state) {
    (void)s; (void)media; (void)dir; (void)index; (void)state; return kResultOk;
}
static tresult c_setActive(void* s, TBool state) { (void)s; (void)state; return kResultOk; }
static tresult c_setState(void* s, void* stream);
static tresult c_getState(void* s, void* stream);

/* ---- IAudioProcessor ---- */
static tresult ap_setBusArrangements(void* s, SpeakerArrangement* ins, int32_t nIn, SpeakerArrangement* outs, int32_t nOut) {
    (void)s; (void)ins; (void)nIn; (void)outs; (void)nOut; return kResultOk;
}
static tresult ap_getBusArrangement(void* s, BusDirection dir, int32_t index, SpeakerArrangement* arr) {
    (void)s; (void)dir; (void)index;
    if (arr) *arr = 3; /* kStereo */
    return kResultOk;
}
static tresult ap_canProcessSampleSize(void* s, int32_t size) { return size == 0 ? kResultOk : kResultFalse; } /* kSample32 */
static uint32_t ap_getLatencySamples(void* s) { (void)s; return 0; }
static tresult ap_setupProcessing(void* s, ProcessSetup* setup) {
    Fixture* f = FX(s);
    if (setup) f->sample_rate = setup->sampleRate;
    return kResultOk;
}
static tresult ap_setProcessing(void* s, TBool state) { (void)s; (void)state; return kResultOk; }
static tresult ap_process(void* s, ProcessData* d) {
    Fixture* f = FX(s);
    if (!d) return kResultOk;
    void* pc = d->inputParameterChanges;
    if (pc) {
        void** vt = *(void***)pc;
        int32_t n = ((int32_t (*)(void*))vt[3])(pc);
        for (int32_t i = 0; i < n; i++) {
            void* q = ((void* (*)(void*, int32_t))vt[4])(pc, i);
            if (!q) continue;
            void** qv = *(void***)q;
            uint32_t id = ((uint32_t (*)(void*))qv[3])(q);
            int32_t cnt = ((int32_t (*)(void*))qv[4])(q);
            double last = 0.0;
            for (int32_t j = 0; j < cnt; j++) {
                int32_t off;
                if (((tresult (*)(void*, int32_t, int32_t*, double*))qv[5])(q, j, &off, &last) != kResultOk) last = 0.0;
            }
            if (id == PID_KIT) f->kit = clamp_kit(last);
            else if (id == PID_PATCH) f->patch = clamp_patch(last);
            else if (id == PID_LEVEL) f->level = last;
        }
    }
    /* silence every configured channel (pins verify state, not sound) */
    for (int32_t b = 0; b < d->numOutputs; b++) {
        AudioBusBuffers* bus = (AudioBusBuffers*)d->outputs + b;
        float** ch = (float**)bus->channelBuffers;
        for (int32_t c = 0; c < bus->numChannels; c++)
            if (ch && ch[c]) memset(ch[c], 0, (size_t)d->numSamples * sizeof(float));
    }
    return kResultOk;
}
static uint32_t ap_getTailSamples(void* s) { (void)s; return 0; }

/* ---- IEditController (+ IPluginBase prefix) ---- */
static tresult ec_setComponentState(void* s, void* st) { (void)s; (void)st; return kNotImplemented; }
static int32_t ec_getParameterCount(void* s) { (void)s; return 3; }
static tresult ec_getParameterInfo(void* s, int32_t index, ParameterInfo* info) {
    (void)s;
    if (!info || index < 0 || index > 2) return kResultFalse;
    memset(info, 0, sizeof(*info));
    switch (index) {
        case 0:
            info->id = PID_LEVEL;
            put_name(info->title, "level"); put_name(info->shortTitle, "lvl"); put_name(info->units, "%");
            info->defaultNormalizedValue = 0.5;
            info->flags = kCanAutomate;
            info->unitId = 0;
            break;
        case 1:
            info->id = PID_KIT;
            put_name(info->title, "kit"); put_name(info->shortTitle, "kit"); put_name(info->units, "");
            info->stepCount = N_DRUMS - 1;
            info->defaultNormalizedValue = 0.0;
            info->flags = kIsProgramChange | kIsList | kIsHidden;
            info->unitId = 0;
            break;
        default:
            info->id = PID_PATCH;
            put_name(info->title, "patch"); put_name(info->shortTitle, "pat"); put_name(info->units, "");
            info->stepCount = N_SYNTH - 1;
            info->defaultNormalizedValue = 0.0;
            info->flags = kIsProgramChange | kIsList | kIsHidden;
            info->unitId = 1;
            break;
    }
    return kResultOk;
}
static tresult ec_getParamStringByValue(void* s, ParamID id, ParamValue v, TChar out[kStringSize]) {
    (void)s;
    if (!out) return kResultFalse;
    if (id == PID_KIT) { put_name(out, kDrumNames[clamp_kit(v)]); return kResultOk; }
    if (id == PID_PATCH) { put_name(out, kSynthNames[clamp_patch(v)]); return kResultOk; }
    return kNotImplemented;
}
static tresult ec_getParamValueByString(void* s, ParamID id, TChar* str, ParamValue* v) { (void)s; (void)id; (void)str; (void)v; return kNotImplemented; }
static ParamValue ec_normalizedParamToPlain(void* s, ParamID id, ParamValue v) { (void)s; (void)id; return v; }
static ParamValue ec_plainParamToNormalized(void* s, ParamID id, ParamValue v) { (void)s; (void)id; return v; }
static ParamValue ec_getParamNormalized(void* s, ParamID id) {
    Fixture* f = FX(s);
    if (id == PID_KIT) return (double)f->kit / (double)(N_DRUMS - 1);
    if (id == PID_PATCH) return (double)f->patch / (double)(N_SYNTH - 1);
    if (id == PID_LEVEL) return f->level;
    return 0.0;
}
static tresult ec_setParamNormalized(void* s, ParamID id, ParamValue v) {
    Fixture* f = FX(s);
    if (id == PID_KIT) { f->kit = clamp_kit(v); return kResultOk; }
    if (id == PID_PATCH) { f->patch = clamp_patch(v); return kResultOk; }
    if (id == PID_LEVEL) { f->level = v; return kResultOk; }
    return kResultFalse;
}
static tresult ec_setComponentHandler(void* s, void* h) { (void)s; (void)h; return kResultOk; }
static void* ec_createView(void* s, const char* name) { (void)s; (void)name; return 0; }

/* ---- IUnitInfo ---- */
static int32_t ui_getUnitCount(void* s) { (void)s; return 2; }
static tresult ui_getUnitInfo(void* s, int32_t index, UnitInfo* info) {
    (void)s;
    if (!info || index < 0 || index > 1) return kResultFalse;
    memset(info, 0, sizeof(*info));
    if (index == 0) { info->id = 0; info->parentUnitId = -1; info->programListId = LIST_DRUMS; put_name(info->name, "drums"); }
    else { info->id = 1; info->parentUnitId = 0; info->programListId = LIST_SYNTH; put_name(info->name, "synth"); }
    return kResultOk;
}
static int32_t ui_getProgramListCount(void* s) { (void)s; return 2; }
static tresult ui_getProgramListInfo(void* s, int32_t index, ProgramListInfo* info) {
    (void)s;
    if (!info || index < 0 || index > 1) return kResultFalse;
    memset(info, 0, sizeof(*info));
    if (index == 0) { info->id = LIST_DRUMS; info->programCount = N_DRUMS; put_name(info->name, "drum kits"); }
    else { info->id = LIST_SYNTH; info->programCount = N_SYNTH; put_name(info->name, "synth patches"); }
    return kResultOk;
}
static tresult ui_getProgramName(void* s, ProgramListID listId, int32_t index, TChar out[kStringSize]) {
    (void)s;
    if (!out) return kResultFalse;
    if (listId == LIST_DRUMS && index >= 0 && index < N_DRUMS) { put_name(out, kDrumNames[index]); return kResultOk; }
    if (listId == LIST_SYNTH && index >= 0 && index < N_SYNTH) { put_name(out, kSynthNames[index]); return kResultOk; }
    return kResultFalse;
}
static tresult ui_getProgramInfo(void* s, ProgramListID l, int32_t i, const TChar* a, TChar* v) { (void)s; (void)l; (void)i; (void)a; (void)v; return kNotImplemented; }
static tresult ui_hasProgramPitchNames(void* s, ProgramListID l, int32_t i) { (void)s; (void)l; (void)i; return kResultFalse; }
static tresult ui_getProgramPitchName(void* s, ProgramListID l, int32_t i, int16_t p, TChar* out) { (void)s; (void)l; (void)i; (void)p; (void)out; return kNotImplemented; }
static UnitID ui_getSelectedUnit(void* s) { (void)s; return 0; }
static tresult ui_selectUnit(void* s, UnitID u) { (void)s; (void)u; return kResultOk; }
static tresult ui_getUnitByBus(void* s, MediaType t, BusDirection d, int32_t b, int32_t c, UnitID* out) { (void)s; (void)t; (void)d; (void)b; (void)c; (void)out; return kNotImplemented; }
static tresult ui_setUnitProgramData(void* s, int32_t l, int32_t i, void* data) { (void)s; (void)l; (void)i; (void)data; return kNotImplemented; }

/* ---- FUnknown implementation (after slots exist; dispatch by offset) ---- */
static tresult unk_qi(void* self0, const uint8_t iid[16], void** obj) {
    Slot* slot = (Slot*)self0;
    Fixture* f = slot->self;
    if (!obj) return kResultFalse;
    *obj = 0;
    if (!memcmp(iid, IID_FUNKNOWN, 16) || !memcmp(iid, IID_IPLUGINBASE, 16) || !memcmp(iid, IID_ICOMPONENT, 16)) *obj = &f->comp;
    else if (!memcmp(iid, IID_IAUDIOPROCESSOR, 16)) *obj = &f->ap;
    else if (!memcmp(iid, IID_IEDITCONTROLLER, 16)) *obj = &f->ec;
    else if (!memcmp(iid, IID_IUNITINFO, 16)) *obj = &f->ui;
    else return kNoInterface;
    f->refs++;
    return kResultOk;
}
static uint32_t unk_addref(void* self0) { return (uint32_t)(++FX(self0)->refs); }
static uint32_t unk_release(void* self0) {
    Fixture* f = FX(self0);
    int r = --f->refs;
    if (r == 0) free(f);
    return (uint32_t)r;
}

/* ---- state streams (component + controller share them) ---- */
static tresult stream_write(void* st, const void* buf, uint64_t size, uint64_t* written) {
    void** vt = *(void***)st;
    return ((tresult (*)(void*, const void*, uint64_t, uint64_t*))vt[4])(st, buf, size, written);
}
static tresult stream_read(void* st, void* buf, uint64_t size, uint64_t* got) {
    void** vt = *(void***)st;
    return ((tresult (*)(void*, void*, uint64_t, uint64_t*))vt[3])(st, buf, size, got);
}
static tresult c_setState(void* s, void* stream) {
    Fixture* f = FX(s);
    if (!stream) return kResultFalse;
    int32_t kit = 0, patch = 0; double level = 0.5; uint64_t got = 0;
    if (stream_read(stream, &kit, 4, &got) != kResultOk) return kResultFalse;
    if (stream_read(stream, &patch, 4, &got) != kResultOk) return kResultFalse;
    if (stream_read(stream, &level, 8, &got) != kResultOk) return kResultFalse;
    if (kit < 0) kit = 0;
    if (kit > N_DRUMS - 1) kit = N_DRUMS - 1;
    if (patch < 0) patch = 0;
    if (patch > N_SYNTH - 1) patch = N_SYNTH - 1;
    f->kit = kit; f->patch = patch; f->level = level;
    return kResultOk;
}
static tresult c_getState(void* s, void* stream) {
    Fixture* f = FX(s);
    if (!stream) return kResultFalse;
    int32_t kit = (int32_t)f->kit, patch = (int32_t)f->patch; double level = f->level; uint64_t written = 0;
    if (stream_write(stream, &kit, 4, &written) != kResultOk) return kResultFalse;
    if (stream_write(stream, &patch, 4, &written) != kResultOk) return kResultFalse;
    if (stream_write(stream, &level, 8, &written) != kResultOk) return kResultFalse;
    return kResultOk;
}

/* ---- vtables (slot order MUST equal the bindings / official headers) ---- */
static const void* VT_COMP[14] = {
    (void*)unk_qi, (void*)unk_addref, (void*)unk_release,
    (void*)c_initialize, (void*)c_terminate,
    (void*)c_getControllerClassId, (void*)c_setIoMode, (void*)c_getBusCount, (void*)c_getBusInfo,
    (void*)c_getRoutingInfo, (void*)c_activateBus, (void*)c_setActive, (void*)c_setState, (void*)c_getState,
};
static const void* VT_AP[11] = {
    (void*)unk_qi, (void*)unk_addref, (void*)unk_release,
    (void*)ap_setBusArrangements, (void*)ap_getBusArrangement, (void*)ap_canProcessSampleSize,
    (void*)ap_getLatencySamples, (void*)ap_setupProcessing, (void*)ap_setProcessing,
    (void*)ap_process, (void*)ap_getTailSamples,
};
static const void* VT_EC[18] = {
    (void*)unk_qi, (void*)unk_addref, (void*)unk_release,
    (void*)c_initialize, (void*)c_terminate,
    (void*)ec_setComponentState, (void*)c_setState, (void*)c_getState,
    (void*)ec_getParameterCount, (void*)ec_getParameterInfo, (void*)ec_getParamStringByValue,
    (void*)ec_getParamValueByString, (void*)ec_normalizedParamToPlain, (void*)ec_plainParamToNormalized,
    (void*)ec_getParamNormalized, (void*)ec_setParamNormalized, (void*)ec_setComponentHandler,
    (void*)ec_createView,
};
static const void* VT_UI[15] = {
    (void*)unk_qi, (void*)unk_addref, (void*)unk_release,
    (void*)ui_getUnitCount, (void*)ui_getUnitInfo, (void*)ui_getProgramListCount, (void*)ui_getProgramListInfo,
    (void*)ui_getProgramName, (void*)ui_getProgramInfo, (void*)ui_hasProgramPitchNames,
    (void*)ui_getProgramPitchName, (void*)ui_getSelectedUnit, (void*)ui_selectUnit,
    (void*)ui_getUnitByBus, (void*)ui_setUnitProgramData,
};

/* ---- factory (a static singleton COM object) ---- */
static tresult fac_qi(void* s, const uint8_t iid[16], void** obj) {
    (void)s;
    if (!obj) return kResultFalse;
    *obj = 0;
    if (!memcmp(iid, IID_FUNKNOWN, 16) || !memcmp(iid, IID_IPLUGINFACTORY, 16)) { *obj = (void*)s; return kResultOk; }
    return kNoInterface;
}
static uint32_t fac_addref(void* s) { (void)s; return 2; }
static uint32_t fac_release(void* s) { (void)s; return 1; }
static tresult fac_getFactoryInfo(void* s, PFactoryInfo* info) {
    (void)s;
    if (!info) return kResultFalse;
    memset(info, 0, sizeof(*info));
    memcpy(info->vendor, "Slooper Fixture", 15);
    memcpy(info->url, "https://github.com/actually-god/slooper", 39);
    memcpy(info->email, "dev@example.invalid", 19);
    info->flags = 0;
    return kResultOk;
}
static int32_t fac_countClasses(void* s) { (void)s; return 1; }
static tresult fac_getClassInfo(void* s, int32_t index, PClassInfo* info) {
    (void)s;
    if (!info || index != 0) return kResultFalse;
    memset(info, 0, sizeof(*info));
    memcpy(info->cid, CLASS_UID, 16);
    info->cardinality = 2147483647; /* kManyInstances */
    memcpy(info->category, "Audio Module Class", 18);
    memcpy(info->name, "V78 Multi", 9);
    return kResultOk;
}
static tresult fac_createInstance(void* s, const char* cid, const char* iid, void** obj) {
    (void)s;
    if (!obj) return kResultFalse;
    *obj = 0;
    if (memcmp(cid, CLASS_UID, 16) != 0) return kResultFalse;
    Fixture* f = (Fixture*)calloc(1, sizeof(Fixture));
    if (!f) return kResultFalse;
    f->refs = 1; f->kit = 0; f->patch = 0; f->level = 0.5; f->sample_rate = 44100.0;
    f->comp.vt = VT_COMP; f->comp.self = f;
    f->ap.vt = VT_AP; f->ap.self = f;
    f->ec.vt = VT_EC; f->ec.self = f;
    f->ui.vt = VT_UI; f->ui.self = f;
    *obj = &f->comp; /* host requested IComponent; same pointer as QI */
    (void)iid;
    return kResultOk;
}
static const void* VT_FAC[7] = {
    (void*)fac_qi, (void*)fac_addref, (void*)fac_release,
    (void*)fac_getFactoryInfo, (void*)fac_countClasses, (void*)fac_getClassInfo, (void*)fac_createInstance,
};
static Slot g_factory = { VT_FAC, 0 };

/* ---- module exports (Linux: ModuleEntry/ModuleExit REQUIRED by the
 * host loader; Windows: GetPluginFactory REQUIRED, the rest harmless) ---- */
FX_API int ModuleEntry(void* handle) { (void)handle; return 1; }
FX_API void ModuleExit(void) {}
FX_API void* GetPluginFactory(void) { return (void*)&g_factory; }
