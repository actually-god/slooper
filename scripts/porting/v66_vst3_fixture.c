/* v66_vst3_fixture.c — a minimal, dependency-free VST3 plugin in raw COM C.
 *
 * Purpose: give slooper's VST3 program surface (v66) a REAL plugin to talk
 * to in headless pins, the same role w6_vst2_fixture.c plays for VST2.
 * It exposes an IUnitInfo program list + a kIsProgramChange parameter — the
 * exact surface vst3-host 0.10 enumerates (get_units / program-change cache)
 * — and applies program changes through BOTH routes the host can take:
 *   - controller setParamNormalized  (host open-time restore, exclusive
 *     handle before the RT runner owns the plugin)
 *   - ProcessData.inputParameterChanges (runtime selection: the host writes
 *     the mirror, poll_params relays through the RealtimePluginRunner ring)
 * Both converge on ONE program variable, so pins can observe either path
 * by re-opening or via the host's cached snapshot.
 *
 * ABI source of truth: the `vst3` 0.3.0 Rust bindings vst3-host compiles
 * against (struct layouts, vtable slot order, IID byte order from
 * support::uid()), cross-checked against the official vst3_pluginterfaces
 * headers. This file MUST keep matching the bindings, not a newer SDK.
 *
 * Surface: one class "V66 Prog" (Audio Module Class). Params:
 *   id 100  "level"   continuous, unit 0
 *   id 1000 "program" kIsProgramChange|kIsHidden, unit 0
 * Units: 0 "root" (program list 7) / 1 "osc" parent 0, no list.
 * Program list 7 "v66 presets": init, pluck, bass, lead.
 * getState/setState carry (int32 program, double level) so chunk rebuilds
 * keep the program. process() outputs silence.
 *
 * v165 EXTENSION — MIDI observer (w6-style, for the VST3 MIDI-wire pins):
 * process() scans ProcessData.inputEvents (crate IEventList vtable:
 * getEventCount @ slot 3, getEvent @ slot 4) and records every raw
 * kLegacyMIDICCOutEvent (type 65535; legacy controlNumber scheme
 * 128=chAT, 129=bend LSB/MSB, 130=PC, else CC) plus a native-event count
 * as a control channel. getState appends the 13 int32 observer fields
 * AFTER the original 12-byte program+level block (append-only keeps the
 * legacy field offsets; slooper's v66_state guard is re-targeted to the
 * new exact length in the same commit).
 *
 * Build: scripts/porting/v66_build_fixtures.sh (host cc + llvm-mingw gcc,
 * same pattern as w6 — static-linked on Windows so a clean CI runner loads
 * it). NO C++ runtime, NO SDK — plain C like the VST2 fixture.
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

/* IIDs per PLATFORM layout: the host compares the `vst3` 0.3.0 bindings'
 * `support::uid(a,b,c,d)` bytes, and uid() is OS-dependent — Windows is
 * the classic mixed-endian GUID form (the bytes everyone knows from the
 * SDK headers); non-Windows vst3 TUIDs are pure big-endian per 32-bit
 * group (uid_impl cfg(not(windows))). Discovered live on 2026-11-16:
 * with only the Windows bytes the Linux host QI-fails with "Component
 * does not implement IAudioProcessor" (createInstance never memcmps the
 * iid, so the first real comparison is that cast). */
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
#define PID_PROGRAM 1000u
#define LIST_ID 7
#define N_PROGRAMS 4
static const char* kProgramNames[N_PROGRAMS] = {"init", "pluck", "bass", "lead"};
/* class uid: fixed 16 bytes (format is opaque — host only echoes it). */
static const uint8_t CLASS_UID[16] = {'V','6','6','p','r','o','g','1','0','1','2','3','4','5','6','7'};

/* ---- host-side interfaces we CALL (queue objects the host owns) ---- */
/* IParameterChanges vtbl: QI, addRef, release, getParameterCount(3),
 *   getParameterData(4, index) -> IParamValueQueue* (borrowed, no addref) */
/* IParamValueQueue vtbl: QI, addRef, release, getParameterId(3)->u32,
 *   getPointCount(4)->i32, getPoint(5, idx, int32* off, double* val) */
/* IBStream vtbl: QI, addRef, release, read(3, this, void* n, size, size_t*
 *   got), write(4, this, const void*, size, size_t* written) */

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

/* v165: mirror of the `vst3` 0.3.0 bindings' Event (repr C:
 * i32 bus, i32 sampleOffset, f64 ppq, u16 flags, u16 type, union @24).
 * The crate copies its own 56-byte Event into whatever pointer getEvent
 * hands it, so this buffer must be AT LEAST that big: 24 + 64 = 88.
 * LegacyMIDICCOutEvent lives at payload offset 0 (4 x int8-ish:
 * controlNumber, channel, value, value2). */
typedef struct {
    int32_t busIndex;
    int32_t sampleOffset;
    double ppqPosition;
    uint16_t flags;
    uint16_t type;
    uint8_t pad[4];
    uint8_t payload[64];
} VEvent;
#define kLegacyMidiType 65535u /* EventTypes::kLegacyMIDICCOutEvent */
typedef char char8;
typedef struct { char8 vendor[64]; char8 url[256]; char8 email[128]; int32_t flags; } PFactoryInfo;
typedef struct { uint8_t cid[16]; int32_t cardinality; char8 category[32]; char8 name[64]; } PClassInfo;

/* ---- the instance ---- */
typedef struct Fixture Fixture;
typedef struct { const void** vt; Fixture* self; } Slot;

struct Fixture {
    int refs;
    int program;      /* shared: controller AND processor converge here */
    double level;
    double sample_rate;
    /* v165 MIDI observer: raw legacy-MIDI (type 65535) tallies from
     * inputEvents + last legacy quadruple + value sums (pin content
     * proof), plus native note/polyAT count as a control. */
    int32_t mo_legacy, mo_cc, mo_bend, mo_at, mo_pc, mo_native;
    int32_t mo_num, mo_ch, mo_val, mo_val2;
    int32_t mo_cc_sum, mo_bend_lsb, mo_bend_msb;
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

static int clamp_program(double norm) {
    double s = norm * (double)(N_PROGRAMS - 1);
    int p = (int)(s + (s >= 0 ? 0.5 : -0.5));
    if (p < 0) p = 0;
    if (p > N_PROGRAMS - 1) p = N_PROGRAMS - 1;
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
            if (id == PID_PROGRAM) f->program = clamp_program(last);
            else if (id == PID_LEVEL) f->level = last;
        }
    }
    /* v165 MIDI observer: tally raw legacy-MIDI bytes in inputEvents.
     * IEventList vtable (crate bindings): QI/addRef/release, then
     * getEventCount(3), getEvent(4). */
    void* evl = d->inputEvents;
    if (evl) {
        void** evt = *(void***)evl;
        int32_t n = ((int32_t (*)(void*))evt[3])(evl);
        for (int32_t i = 0; i < n; i++) {
            VEvent e; memset(&e, 0, sizeof e);
            if (((tresult (*)(void*, int32_t, void*))evt[4])(evl, i, &e) != kResultOk) continue;
            if (e.type == kLegacyMidiType) {
                int32_t num = e.payload[0], ch = (int32_t)e.payload[1],
                        val = (int32_t)e.payload[2], val2 = (int32_t)e.payload[3];
                f->mo_legacy++;
                f->mo_num = num; f->mo_ch = ch; f->mo_val = val; f->mo_val2 = val2;
                if (num == 129) { f->mo_bend++; f->mo_bend_lsb += val; f->mo_bend_msb += val2; }
                else if (num == 128) f->mo_at++;
                else if (num == 130) f->mo_pc++;
                else { f->mo_cc++; f->mo_cc_sum += val; }
            } else if (e.type == 0 || e.type == 1 || e.type == 3) {
                f->mo_native++; /* noteOn / noteOff / polyPressure (this crate's IDs) */
            }
        }
    }
    /* silence every configured channel (a silent instrument is fine:
     * pins verify state, and slooper's own audio tests use real plugins) */
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
static int32_t ec_getParameterCount(void* s) { (void)s; return 2; }
static tresult ec_getParameterInfo(void* s, int32_t index, ParameterInfo* info) {
    (void)s;
    if (!info || index < 0 || index > 1) return kResultFalse;
    memset(info, 0, sizeof(*info));
    if (index == 0) {
        info->id = PID_LEVEL;
        put_name(info->title, "level"); put_name(info->shortTitle, "lvl"); put_name(info->units, "%");
        info->defaultNormalizedValue = 0.5;
        info->flags = kCanAutomate;
    } else {
        info->id = PID_PROGRAM;
        put_name(info->title, "program"); put_name(info->shortTitle, "prg"); put_name(info->units, "");
        info->stepCount = N_PROGRAMS - 1;
        info->defaultNormalizedValue = 0.0;
        info->flags = kIsProgramChange | kIsList | kIsHidden;
    }
    info->unitId = 0;
    return kResultOk;
}
static tresult ec_getParamStringByValue(void* s, ParamID id, ParamValue v, TChar out[kStringSize]) {
    (void)s;
    if (id == PID_PROGRAM && out) { put_name(out, kProgramNames[clamp_program(v)]); return kResultOk; }
    return kNotImplemented;
}
static tresult ec_getParamValueByString(void* s, ParamID id, TChar* str, ParamValue* v) { (void)s; (void)id; (void)str; (void)v; return kNotImplemented; }
static ParamValue ec_normalizedParamToPlain(void* s, ParamID id, ParamValue v) { (void)s; (void)id; return v; }
static ParamValue ec_plainParamToNormalized(void* s, ParamID id, ParamValue v) { (void)s; (void)id; return v; }
static ParamValue ec_getParamNormalized(void* s, ParamID id) {
    Fixture* f = FX(s);
    if (id == PID_PROGRAM) return (double)f->program / (double)(N_PROGRAMS - 1);
    if (id == PID_LEVEL) return f->level;
    return 0.0;
}
static tresult ec_setParamNormalized(void* s, ParamID id, ParamValue v) {
    Fixture* f = FX(s);
    if (id == PID_PROGRAM) { f->program = clamp_program(v); return kResultOk; }
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
    if (index == 0) { info->id = 0; info->parentUnitId = -1; info->programListId = LIST_ID; put_name(info->name, "root"); }
    else { info->id = 1; info->parentUnitId = 0; info->programListId = -1; put_name(info->name, "osc"); }
    return kResultOk;
}
static int32_t ui_getProgramListCount(void* s) { (void)s; return 1; }
static tresult ui_getProgramListInfo(void* s, int32_t index, ProgramListInfo* info) {
    (void)s;
    if (!info || index != 0) return kResultFalse;
    memset(info, 0, sizeof(*info));
    info->id = LIST_ID;
    info->programCount = N_PROGRAMS;
    put_name(info->name, "v66 presets");
    return kResultOk;
}
static tresult ui_getProgramName(void* s, ProgramListID listId, int32_t index, TChar out[kStringSize]) {
    (void)s;
    if (listId != LIST_ID || index < 0 || index >= N_PROGRAMS || !out) return kResultFalse;
    put_name(out, kProgramNames[index]);
    return kResultOk;
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
    int32_t prog = 0; double level = 0.5; uint64_t got = 0;
    if (stream_read(stream, &prog, 4, &got) != kResultOk) return kResultFalse;
    if (stream_read(stream, &level, 8, &got) != kResultOk) return kResultFalse;
    if (prog < 0) prog = 0;
    if (prog > N_PROGRAMS - 1) prog = N_PROGRAMS - 1;
    f->program = prog; f->level = level;
    return kResultOk;
}
static tresult c_getState(void* s, void* stream) {
    Fixture* f = FX(s);
    if (!stream) return kResultFalse;
    int32_t prog = (int32_t)f->program; double level = f->level; uint64_t written = 0;
    if (stream_write(stream, &prog, 4, &written) != kResultOk) return kResultFalse;
    if (stream_write(stream, &level, 8, &written) != kResultOk) return kResultFalse;
    /* v165 observer block: 13 x int32, order MUST match v165_midi_state
     * in src/vsthost.rs: legacy, cc, bend, at, pc, native, num, ch, val,
     * val2, cc_sum, bend_lsb, bend_msb. */
    int32_t obs[13] = { f->mo_legacy, f->mo_cc, f->mo_bend, f->mo_at, f->mo_pc,
                        f->mo_native, f->mo_num, f->mo_ch, f->mo_val, f->mo_val2,
                        f->mo_cc_sum, f->mo_bend_lsb, f->mo_bend_msb };
    if (stream_write(stream, obs, (int32_t)sizeof(obs), &written) != kResultOk) return kResultFalse;
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
    memcpy(info->name, "V66 Prog", 8);
    return kResultOk;
}
static tresult fac_createInstance(void* s, const char* cid, const char* iid, void** obj) {
    (void)s;
    if (!obj) return kResultFalse;
    *obj = 0;
    if (memcmp(cid, CLASS_UID, 16) != 0) return kResultFalse;
    Fixture* f = (Fixture*)calloc(1, sizeof(Fixture));
    if (!f) return kResultFalse;
    f->refs = 1; f->program = 0; f->level = 0.5; f->sample_rate = 44100.0;
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
