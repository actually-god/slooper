// v116: throw-class CLAP fixture — FOUR deterministic plugins in one DSO,
// each raising a real C++ exception from ONE designated ABI call site, to
// pin the ffiguard firewall over the CLAP backend (the v42 doctrine the
// v34 backend had not inherited; card t_cb95ff24).
//
//   com.slooper.fixture-throw-init  -> clap_plugin.init() throws
//       (covers the whole open_unlocked guard: create -> init leg)
//   com.slooper.fixture-throw-proc  -> clap_plugin.process() throws
//       (covers the RT-thread guard: entry must PARK, not terminate)
//   com.slooper.fixture-throw-state -> clap.state save() throws
//       (chunk harvest: host must degrade to "no state", not die)
//   com.slooper.fixture-throw-load  -> clap.preset-load/2 from_location
//       throws (host-commanded load must surface Err)
//   SLOOPER_CLAP_FIXTURE_THROW_PROBE=1 at probe time ->
//       plugin-factory.get_plugin_count throws (scan path guard; env-gated
//       because open() also walks the factory — the gate scopes the throw
//       to exactly the probe pin).
//   DSO-level preset-discovery factory count() throws UNCONDITIONALLY —
//   the host only ever calls it from preset_locations() (lazy discovery),
//   which is exactly the guarded site being pinned.
//
// The throws MUST be C++ (std::runtime_error): the whole point is a
// foreign exception unwinding through our Rust frames into the fg_try_call
// catch. This file therefore compiles as C++ ($CXX||c++), unlike
// clap_fixture.c. Test infrastructure for src/claphost.rs; MIT like the host.

#include <cstdlib>
#include <cstring>
#include <stdexcept>

#include <clap/clap.h>

static bool g_env_set(const char *name) {
   const char *v = std::getenv(name);
   return v != nullptr && v[0] != '\0' && std::strcmp(v, "0") != 0;
}

enum Kind {
   KIND_INIT_THROW,
   KIND_PROC_THROW,
   KIND_STATE_THROW,
   KIND_LOAD_THROW,
};

struct throw_fx {
   clap_plugin_t plugin; // must stay first: plugin_data casts back to this
   const clap_host_t *host;
   Kind kind;
};

// ---- descriptors (positional init: clap_version, id, name, vendor, url,
// ---- manual, support, version, description, features) ----
#define THROW_DESC(sym, idstr, namestr)                                      \
   static const clap_plugin_descriptor_t sym = {                             \
      CLAP_VERSION_INIT, idstr, namestr, "slooper", nullptr, nullptr,        \
      nullptr, "1.0", "v116 throw-class CLAP fixture", nullptr };

THROW_DESC(s_desc_init, "com.slooper.fixture-throw-init", "Fixture ThrowInit")
THROW_DESC(s_desc_proc, "com.slooper.fixture-throw-proc", "Fixture ThrowProc")
THROW_DESC(s_desc_state, "com.slooper.fixture-throw-state", "Fixture ThrowState")
THROW_DESC(s_desc_load, "com.slooper.fixture-throw-load", "Fixture ThrowLoad")

static throw_fx g_fx_init;
static throw_fx g_fx_proc;
static throw_fx g_fx_state;
static throw_fx g_fx_load;

// ---- plugin vtable arms ----
static bool p_init(const clap_plugin_t *p) {
   const throw_fx *fx = static_cast<const throw_fx *>(p->plugin_data);
   if (fx->kind == KIND_INIT_THROW)
      throw std::runtime_error("slooper clap fixture throw: init");
   return true;
}
static void p_destroy(const clap_plugin_t *) {}
static bool p_activate(const clap_plugin_t *, double, uint32_t, uint32_t) { return true; }
static void p_deactivate(const clap_plugin_t *) {}
static bool p_start_processing(const clap_plugin_t *) { return true; }
static void p_stop_processing(const clap_plugin_t *) {}
static void p_reset(const clap_plugin_t *) {}
static clap_process_status p_process(const clap_plugin_t *p, const clap_process_t *) {
   const throw_fx *fx = static_cast<const throw_fx *>(p->plugin_data);
   if (fx->kind == KIND_PROC_THROW)
      throw std::runtime_error("slooper clap fixture throw: process");
   return CLAP_PROCESS_CONTINUE;
}
static void p_on_main_thread(const clap_plugin_t *) {}

// clap.state: only ever vended to the throw-state plugin.
static bool state_save(const clap_plugin_t *, const clap_ostream_t *) {
   throw std::runtime_error("slooper clap fixture throw: state-save");
}
static bool state_load(const clap_plugin_t *, const clap_istream_t *) { return true; }
static const clap_plugin_state_t kStateVt = { state_save, state_load };

// clap.preset-load/2: only ever vended to the throw-load plugin.
static bool pl_from_location(const clap_plugin_t *, uint32_t, const char *, const char *) {
   throw std::runtime_error("slooper clap fixture throw: preset-load");
}
static const clap_plugin_preset_load_t kPresetLoadVt = { pl_from_location };

static const void *p_get_extension(const clap_plugin_t *p, const char *id) {
   const throw_fx *fx = static_cast<const throw_fx *>(p->plugin_data);
   if (fx->kind == KIND_STATE_THROW && std::strcmp(id, CLAP_EXT_STATE) == 0)
      return &kStateVt;
   if (fx->kind == KIND_LOAD_THROW &&
       (std::strcmp(id, CLAP_EXT_PRESET_LOAD) == 0 ||
        std::strcmp(id, CLAP_EXT_PRESET_LOAD_COMPAT) == 0))
      return &kPresetLoadVt;
   return nullptr;
}

static void setup(throw_fx &fx, const clap_plugin_descriptor_t *d, Kind kind) {
   fx.plugin.desc = d;
   fx.plugin.plugin_data = &fx;
   fx.plugin.init = p_init;
   fx.plugin.destroy = p_destroy;
   fx.plugin.activate = p_activate;
   fx.plugin.deactivate = p_deactivate;
   fx.plugin.start_processing = p_start_processing;
   fx.plugin.stop_processing = p_stop_processing;
   fx.plugin.reset = p_reset;
   fx.plugin.process = p_process;
   fx.plugin.get_extension = p_get_extension;
   fx.plugin.on_main_thread = p_on_main_thread;
   fx.host = nullptr;
   fx.kind = kind;
}

// ---- plugin factory ----
static uint32_t f_count(const clap_plugin_factory_t *) {
   if (g_env_set("SLOOPER_CLAP_FIXTURE_THROW_PROBE"))
      throw std::runtime_error("slooper clap fixture throw: probe-count");
   return 4;
}
static const clap_plugin_descriptor_t *f_desc(const clap_plugin_factory_t *, uint32_t i) {
   static const clap_plugin_descriptor_t *tbl[4] = {
      &s_desc_init, &s_desc_proc, &s_desc_state, &s_desc_load };
   return i < 4 ? tbl[i] : nullptr;
}
static const clap_plugin_t *f_create(const clap_plugin_factory_t *, const clap_host_t *host,
                                     const char *id) {
   if (std::strcmp(id, "com.slooper.fixture-throw-init") == 0) {
      g_fx_init.host = host;
      return &g_fx_init.plugin;
   }
   if (std::strcmp(id, "com.slooper.fixture-throw-proc") == 0) {
      g_fx_proc.host = host;
      return &g_fx_proc.plugin;
   }
   if (std::strcmp(id, "com.slooper.fixture-throw-state") == 0) {
      g_fx_state.host = host;
      return &g_fx_state.plugin;
   }
   if (std::strcmp(id, "com.slooper.fixture-throw-load") == 0) {
      g_fx_load.host = host;
      return &g_fx_load.plugin;
   }
   return nullptr;
}
static const clap_plugin_factory_t kFactory = { f_count, f_desc, f_create };

// ---- preset-discovery factory (count() throws: pins the lazy-discovery
// ---- guard; create/get_descriptor never reached, present for layout) ----
static uint32_t d_count(const clap_preset_discovery_factory_t *) {
   throw std::runtime_error("slooper clap fixture throw: discovery-count");
}
static const clap_preset_discovery_provider_descriptor_t *
d_get_descriptor(const clap_preset_discovery_factory_t *, uint32_t) {
   return nullptr;
}
static const clap_preset_discovery_provider_t *
d_create(const clap_preset_discovery_factory_t *, const clap_preset_discovery_indexer_t *,
         const char *) {
   return nullptr;
}
static const clap_preset_discovery_factory_t kDisc = { d_count, d_get_descriptor, d_create };

// ---- entry ----
static bool g_inited = false;
static bool entry_init(const char *) {
   g_inited = true;
   return true;
}
static void entry_deinit(void) { g_inited = false; }
static const void *entry_get_factory(const char *id) {
   if (std::strcmp(id, CLAP_PLUGIN_FACTORY_ID) == 0) return &kFactory;
   if (std::strcmp(id, CLAP_PRESET_DISCOVERY_FACTORY_ID) == 0 ||
       std::strcmp(id, CLAP_PRESET_DISCOVERY_FACTORY_ID_COMPAT) == 0)
      return &kDisc;
   return nullptr;
}

CLAP_EXPORT const clap_plugin_entry_t clap_entry = {
   CLAP_VERSION_INIT, entry_init, entry_deinit, entry_get_factory };

// Constructors run at dlopen; assign the vtables before any host call.
namespace {
struct Boot {
   Boot() {
      setup(g_fx_init, &s_desc_init, KIND_INIT_THROW);
      setup(g_fx_proc, &s_desc_proc, KIND_PROC_THROW);
      setup(g_fx_state, &s_desc_state, KIND_STATE_THROW);
      setup(g_fx_load, &s_desc_load, KIND_LOAD_THROW);
      (void)g_inited;
   }
} s_boot;
} // namespace
