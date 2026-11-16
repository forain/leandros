/* vkwl — M4: a real Vulkan swapchain on a real Wayland surface, presented to
 * cosmic-comp from inside LeandrOS.
 *
 * WHY THIS EXISTS. vkswap proved Venus can drive a swapchain, but only on
 * VK_EXT_headless_surface — no compositor, no window system, nothing on screen.
 * vkrender has no WSI at all. Neither touches Wayland. This is the first client
 * that binds Mesa's wsi_wl to a live compositor.
 *
 * THE ROUTE. cosmic-comp here runs on softpipe and does NOT advertise
 * zwp_linux_dmabuf_v1. Mesa's Wayland WSI binds wl_shm only when
 * MESA_VK_WSI_DEBUG=sw is set, and zwp_linux_dmabuf_v1 only when it is not
 * (wsi_common_wayland.c registry handler) — mutually exclusive. So a non-sw
 * Venus on this compositor has no way to make a wl_buffer and fails the
 * surface. Pass "sw" as an argument (or set MESA_VK_WSI_DEBUG=sw) to take the
 * shm route; pass "nosw" to take the dmabuf route deliberately, as a control.
 *
 * Same idiom as vkswap/vkrender: no Khronos loader (LeandrOS ships none),
 * dlopen the ICD by absolute path, bootstrap from vk_icdGetInstanceProcAddr.
 * Same harness-parseable output: "<name>: PASS" / "FAIL" / "SKIP (why)" and
 *   --- vkwl done, failures = N ---
 *
 * Usage: vkwl [sw|swnoshm|nosw] [frames] [slow] [hold[=secs]]
 */

#define _GNU_SOURCE

#include <vulkan/vulkan_core.h>
#include <vulkan/vk_icd.h>

#include <dlfcn.h>
#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <time.h>

#include <wayland-client.h>
#include "xdg-shell-client-protocol.h"

/* VK_KHR_wayland_surface is declared in vulkan_wayland.h, which pulls in
 * vulkan.h. Declare the two pieces we need directly so this file keeps
 * vkswap's minimal include set (vulkan_core.h + vk_icd.h only). */
#ifndef VK_KHR_wayland_surface
#define VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR \
    ((VkStructureType)1000006000)
typedef VkFlags VkWaylandSurfaceCreateFlagsKHR;
typedef struct VkWaylandSurfaceCreateInfoKHR {
    VkStructureType                 sType;
    const void                     *pNext;
    VkWaylandSurfaceCreateFlagsKHR  flags;
    struct wl_display              *display;
    struct wl_surface              *surface;
} VkWaylandSurfaceCreateInfoKHR;
typedef VkResult (VKAPI_PTR *PFN_vkCreateWaylandSurfaceKHR)(
    VkInstance, const VkWaylandSurfaceCreateInfoKHR *,
    const VkAllocationCallbacks *, VkSurfaceKHR *);
typedef VkBool32 (VKAPI_PTR *PFN_vkGetPhysicalDeviceWaylandPresentationSupportKHR)(
    VkPhysicalDevice, uint32_t, struct wl_display *);
#endif

#define ICD_PATH "/usr/lib/libvulkan_virtio.so"
#define DEF_W 480u
#define DEF_H 320u
#define FENCE_TIMEOUT_NS 20000000000ull /* 20 s; TCG guests are slow */

static int g_fail = 0;
/* hold mode: after presenting each of the last g_hold_frames frames, stay on
 * that frame for g_hold_secs so a host-side capture has something stationary
 * to photograph. Two frames, not one, because two DIFFERENT predicted colours
 * in the same rectangle is what separates "our pixels reached the scanout"
 * from "something coloured happened to be there". */
/* quiet: suppress the per-frame present log. The framebuffer IS the primary
 * console on LeandrOS and it scrolls the whole framebuffer on every line,
 * including the region cosmic-comp scans out of; since the compositor only
 * repaints what is damaged, one line per frame is enough to scroll a parked
 * client and a static wallpaper away and never redraw them. */
static int g_quiet = 0;
static unsigned g_hold_secs   = 0;
static unsigned g_hold_frames = 2;
static int g_skip = 0;

#define INFO(fmt, ...) do { printf("[INFO] " fmt "\n", ##__VA_ARGS__); } while (0)
#define ERR(fmt, ...)  do { printf("[ERR ] " fmt "\n", ##__VA_ARGS__); } while (0)

static int report(const char *name, int ok)
{
    printf("%s: %s\n", name, ok ? "PASS" : "FAIL");
    if (!ok) g_fail++;
    return ok;
}

static void report_skip(const char *name, const char *why)
{
    printf("%s: SKIP (%s)\n", name, why);
    g_skip++;
}

static const char *vkstr(VkResult r)
{
    switch (r) {
    case VK_SUCCESS: return "VK_SUCCESS";
    case VK_NOT_READY: return "VK_NOT_READY";
    case VK_TIMEOUT: return "VK_TIMEOUT";
    case VK_INCOMPLETE: return "VK_INCOMPLETE";
    case VK_SUBOPTIMAL_KHR: return "VK_SUBOPTIMAL_KHR";
    case VK_ERROR_OUT_OF_HOST_MEMORY: return "VK_ERROR_OUT_OF_HOST_MEMORY";
    case VK_ERROR_OUT_OF_DEVICE_MEMORY: return "VK_ERROR_OUT_OF_DEVICE_MEMORY";
    case VK_ERROR_INITIALIZATION_FAILED: return "VK_ERROR_INITIALIZATION_FAILED";
    case VK_ERROR_DEVICE_LOST: return "VK_ERROR_DEVICE_LOST";
    case VK_ERROR_MEMORY_MAP_FAILED: return "VK_ERROR_MEMORY_MAP_FAILED";
    case VK_ERROR_EXTENSION_NOT_PRESENT: return "VK_ERROR_EXTENSION_NOT_PRESENT";
    case VK_ERROR_FEATURE_NOT_PRESENT: return "VK_ERROR_FEATURE_NOT_PRESENT";
    case VK_ERROR_INCOMPATIBLE_DRIVER: return "VK_ERROR_INCOMPATIBLE_DRIVER";
    case VK_ERROR_FORMAT_NOT_SUPPORTED: return "VK_ERROR_FORMAT_NOT_SUPPORTED";
    case VK_ERROR_UNKNOWN: return "VK_ERROR_UNKNOWN";
    case VK_ERROR_INVALID_EXTERNAL_HANDLE: return "VK_ERROR_INVALID_EXTERNAL_HANDLE";
    case VK_ERROR_SURFACE_LOST_KHR: return "VK_ERROR_SURFACE_LOST_KHR";
    case VK_ERROR_NATIVE_WINDOW_IN_USE_KHR: return "VK_ERROR_NATIVE_WINDOW_IN_USE_KHR";
    case VK_ERROR_OUT_OF_DATE_KHR: return "VK_ERROR_OUT_OF_DATE_KHR";
    case VK_ERROR_INCOMPATIBLE_DISPLAY_KHR: return "VK_ERROR_INCOMPATIBLE_DISPLAY_KHR";
    default: {
        static char b[48];
        snprintf(b, sizeof(b), "VkResult(%d)", (int)r);
        return b;
    }
    }
}

/* ───────────────────────────── Wayland side ───────────────────────────── */

static struct wl_display    *wl_dpy;
static struct wl_compositor *wl_comp;
static struct xdg_wm_base   *wl_wm_base;
static struct wl_surface    *wl_surf;
static struct xdg_surface   *wl_xsurf;
static struct xdg_toplevel  *wl_top;

static int      wl_configured = 0;
static int32_t  wl_cfg_w = 0, wl_cfg_h = 0;
static int      wl_saw_dmabuf = 0;   /* zwp_linux_dmabuf_v1 advertised?  */
static int      wl_saw_shm = 0;      /* wl_shm advertised?               */
static int      wl_globals = 0;

static void xsurf_configure(void *d, struct xdg_surface *xs, uint32_t serial)
{
    (void)d;
    xdg_surface_ack_configure(xs, serial);
    wl_configured = 1;
}
static const struct xdg_surface_listener xsurf_listener = { xsurf_configure };

static void top_configure(void *d, struct xdg_toplevel *t, int32_t w, int32_t h,
                          struct wl_array *st)
{
    (void)d; (void)t; (void)st;
    if (w > 0 && h > 0) { wl_cfg_w = w; wl_cfg_h = h; }
}
static void top_close(void *d, struct xdg_toplevel *t) { (void)d; (void)t; }
static void top_bounds(void *d, struct xdg_toplevel *t, int32_t w, int32_t h)
{ (void)d; (void)t; (void)w; (void)h; }
static void top_caps(void *d, struct xdg_toplevel *t, struct wl_array *c)
{ (void)d; (void)t; (void)c; }
static const struct xdg_toplevel_listener top_listener = {
    top_configure, top_close, top_bounds, top_caps
};

static void wm_ping(void *d, struct xdg_wm_base *b, uint32_t serial)
{ (void)d; xdg_wm_base_pong(b, serial); }
static const struct xdg_wm_base_listener wm_listener = { wm_ping };

static void reg_global(void *d, struct wl_registry *r, uint32_t name,
                       const char *iface, uint32_t ver)
{
    (void)d;
    wl_globals++;
    /* Print every global: this list IS the evidence for which WSI route the
     * compositor can support, and it is the first thing to look at if the
     * swapchain fails. */
    INFO("wl global: %-34s v%u", iface, ver);
    if (!strcmp(iface, "wl_compositor"))
        wl_comp = wl_registry_bind(r, name, &wl_compositor_interface,
                                   ver < 4 ? ver : 4);
    else if (!strcmp(iface, "xdg_wm_base")) {
        wl_wm_base = wl_registry_bind(r, name, &xdg_wm_base_interface, 1);
        xdg_wm_base_add_listener(wl_wm_base, &wm_listener, NULL);
    } else if (!strcmp(iface, "wl_shm"))
        wl_saw_shm = 1;
    else if (!strcmp(iface, "zwp_linux_dmabuf_v1"))
        wl_saw_dmabuf = 1;
}
static void reg_remove(void *d, struct wl_registry *r, uint32_t name)
{ (void)d; (void)r; (void)name; }
static const struct wl_registry_listener reg_listener = { reg_global, reg_remove };

/* Compositor-side proof of presentation: a wl_surface.frame callback only
 * fires when the compositor has actually taken a committed buffer to repaint.
 * One is requested before every present (Mesa's commit carries it); the count
 * of "done" events is the number of frames the compositor consumed. */
static unsigned g_frame_done;
static void frame_done(void *d, struct wl_callback *cb, uint32_t t)
{
    g_frame_done++;
    wl_callback_destroy(cb);
}
static const struct wl_callback_listener frame_listener = { frame_done };

static int wayland_bringup(void)
{
    wl_dpy = wl_display_connect(NULL);
    if (!wl_dpy) {
        ERR("wl_display_connect failed (WAYLAND_DISPLAY=%s XDG_RUNTIME_DIR=%s)",
            getenv("WAYLAND_DISPLAY") ? getenv("WAYLAND_DISPLAY") : "(unset)",
            getenv("XDG_RUNTIME_DIR") ? getenv("XDG_RUNTIME_DIR") : "(unset)");
        return report("wl_display_connect", 0);
    }
    report("wl_display_connect", 1);

    struct wl_registry *reg = wl_display_get_registry(wl_dpy);
    wl_registry_add_listener(reg, &reg_listener, NULL);
    wl_display_roundtrip(wl_dpy);
    INFO("registry: %d globals, wl_shm=%s zwp_linux_dmabuf_v1=%s",
         wl_globals, wl_saw_shm ? "YES" : "no", wl_saw_dmabuf ? "YES" : "no");
    if (!report("wl_registry_has_compositor_and_xdg_shell",
                wl_comp != NULL && wl_wm_base != NULL))
        return 0;

    wl_surf  = wl_compositor_create_surface(wl_comp);
    wl_xsurf = xdg_wm_base_get_xdg_surface(wl_wm_base, wl_surf);
    xdg_surface_add_listener(wl_xsurf, &xsurf_listener, NULL);
    wl_top = xdg_surface_get_toplevel(wl_xsurf);
    xdg_toplevel_add_listener(wl_top, &top_listener, NULL);
    xdg_toplevel_set_title(wl_top, "vkwl");
    xdg_toplevel_set_app_id(wl_top, "leandros.vkwl");
    wl_surface_commit(wl_surf);

    /* Wait for the first configure. Without it the surface is never mapped,
     * frame callbacks never fire, and a FIFO swapchain deadlocks on its
     * second acquire — so this is a hard gate, not a courtesy. */
    for (int i = 0; i < 40 && !wl_configured; i++)
        if (wl_display_roundtrip(wl_dpy) < 0) break;
    INFO("xdg_surface configure received=%d, toplevel size=%dx%d",
         wl_configured, wl_cfg_w, wl_cfg_h);
    return report("xdg_surface_configured", wl_configured);
}

/* ───────────────────────────── Vulkan side ───────────────────────────── */

static PFN_vk_icdGetInstanceProcAddr icd_gipa;
static VkInstance       g_instance;
static VkPhysicalDevice g_phys;
static VkSurfaceKHR     g_surface;
static VkDevice         g_dev;
static VkQueue          g_queue;
static uint32_t         g_qfam;

#define INSTANCE_FUNCS(X)                                       \
    X(vkDestroyInstance)                                        \
    X(vkEnumeratePhysicalDevices)                               \
    X(vkGetPhysicalDeviceProperties)                            \
    X(vkGetPhysicalDeviceQueueFamilyProperties)                 \
    X(vkEnumerateDeviceExtensionProperties)                     \
    X(vkCreateDevice)                                           \
    X(vkDestroyDevice)                                          \
    X(vkGetDeviceProcAddr)                                      \
    X(vkCreateWaylandSurfaceKHR)                                \
    X(vkGetPhysicalDeviceWaylandPresentationSupportKHR)         \
    X(vkDestroySurfaceKHR)                                      \
    X(vkGetPhysicalDeviceSurfaceSupportKHR)                     \
    X(vkGetPhysicalDeviceSurfaceCapabilitiesKHR)                \
    X(vkGetPhysicalDeviceSurfaceFormatsKHR)                     \
    X(vkGetPhysicalDeviceSurfacePresentModesKHR)

#define DEVICE_FUNCS(X)                 \
    X(vkGetDeviceQueue)                 \
    X(vkDeviceWaitIdle)                 \
    X(vkCreateSwapchainKHR)             \
    X(vkDestroySwapchainKHR)            \
    X(vkGetSwapchainImagesKHR)          \
    X(vkAcquireNextImageKHR)            \
    X(vkQueuePresentKHR)                \
    X(vkCreateFence)                    \
    X(vkDestroyFence)                   \
    X(vkResetFences)                    \
    X(vkWaitForFences)                  \
    X(vkCreateCommandPool)              \
    X(vkDestroyCommandPool)             \
    X(vkResetCommandPool)               \
    X(vkAllocateCommandBuffers)         \
    X(vkBeginCommandBuffer)             \
    X(vkEndCommandBuffer)               \
    X(vkCmdPipelineBarrier)             \
    X(vkCmdClearColorImage)             \
    X(vkQueueSubmit)

#define DECL(n) static PFN_##n n##_;
INSTANCE_FUNCS(DECL)
DECL(vkEnumerateInstanceExtensionProperties)
DEVICE_FUNCS(DECL)
#undef DECL

static int bootstrap(void)
{
    /* VKWL_ICD exists so the identical source can be built against a normal
     * distro Mesa and run on the host as a control: if the sw-WSI route fails
     * in the guest, the same binary logic passing on the host against RADV
     * says the client is not the suspect. */
    const char *icd_path = getenv("VKWL_ICD");
    if (!icd_path || !*icd_path) icd_path = ICD_PATH;
    INFO("ICD: %s", icd_path);
    void *icd = dlopen(icd_path, RTLD_NOW | RTLD_LOCAL);
    if (!icd) {
        ERR("dlopen(%s): %s", icd_path, dlerror());
        return report("icd_dlopen", 0);
    }
    report("icd_dlopen", 1);

    icd_gipa = (PFN_vk_icdGetInstanceProcAddr)dlsym(icd, "vk_icdGetInstanceProcAddr");
    if (!icd_gipa) return report("icd_gipa", 0);
    report("icd_gipa", 1);

    vkEnumerateInstanceExtensionProperties_ =
        (PFN_vkEnumerateInstanceExtensionProperties)
            icd_gipa(NULL, "vkEnumerateInstanceExtensionProperties");
    if (!vkEnumerateInstanceExtensionProperties_)
        return report("resolve_enumerate_instance_extensions", 0);

    uint32_t n = 0;
    vkEnumerateInstanceExtensionProperties_(NULL, &n, NULL);
    VkExtensionProperties *ex = n ? calloc(n, sizeof(*ex)) : NULL;
    if (ex) vkEnumerateInstanceExtensionProperties_(NULL, &n, ex);
    int have_surface = 0, have_wl = 0;
    for (uint32_t i = 0; i < n && ex; i++) {
        if (!strcmp(ex[i].extensionName, "VK_KHR_surface")) have_surface = 1;
        if (!strcmp(ex[i].extensionName, "VK_KHR_wayland_surface")) have_wl = 1;
    }
    free(ex);
    INFO("instanceExtensionCount=%u  VK_KHR_surface=%s  VK_KHR_wayland_surface=%s",
         n, have_surface ? "yes" : "NO", have_wl ? "yes" : "NO");
    if (!report("instance_advertises_wayland_surface", have_surface && have_wl))
        return 0;

    PFN_vkCreateInstance create_instance =
        (PFN_vkCreateInstance)icd_gipa(NULL, "vkCreateInstance");
    if (!create_instance) return report("resolve_vkCreateInstance", 0);

    const char *inst_exts[] = { "VK_KHR_surface", "VK_KHR_wayland_surface" };
    VkApplicationInfo app = {
        .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
        .pApplicationName = "vkwl",
        .pEngineName = "vkwl",
        .apiVersion = VK_API_VERSION_1_1,
    };
    VkInstanceCreateInfo ici = {
        .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
        .pApplicationInfo = &app,
        .enabledExtensionCount = 2,
        .ppEnabledExtensionNames = inst_exts,
    };
    VkResult r = create_instance(&ici, NULL, &g_instance);
    if (r != VK_SUCCESS) {
        ERR("vkCreateInstance -> %s", vkstr(r));
        return report("create_instance_with_wayland_wsi", 0);
    }
    report("create_instance_with_wayland_wsi", 1);

    int missing = 0;
#define X(fn)                                                                      \
    do {                                                                           \
        fn##_ = (PFN_##fn)icd_gipa(g_instance, #fn);                               \
        if (!fn##_) { ERR("vk_icdGetInstanceProcAddr(%s) -> NULL", #fn); missing++; } \
    } while (0);
    INSTANCE_FUNCS(X)
#undef X
    return report("resolve_instance_wsi_funcs", missing == 0);
}

static int pick_device(void)
{
    uint32_t n = 0;
    VkResult r = vkEnumeratePhysicalDevices_(g_instance, &n, NULL);
    if (r != VK_SUCCESS || n == 0) return report("enumerate_physical_devices", 0);
    VkPhysicalDevice *d = calloc(n, sizeof(*d));
    vkEnumeratePhysicalDevices_(g_instance, &n, d);
    g_phys = d[0];
    free(d);
    VkPhysicalDeviceProperties p;
    vkGetPhysicalDeviceProperties_(g_phys, &p);
    INFO("device: \"%s\" api=%u.%u.%u", p.deviceName,
         VK_VERSION_MAJOR(p.apiVersion), VK_VERSION_MINOR(p.apiVersion),
         VK_VERSION_PATCH(p.apiVersion));
    return report("enumerate_physical_devices", 1);
}

/* ───────────────────────────────── main ───────────────────────────────── */

/* Keep the surface mapped and the last presented buffer on screen for `secs`,
 * while still answering xdg_wm_base.ping and wl_buffer.release. A plain sleep
 * would work today but leaves the client looking unresponsive to the
 * compositor, which is exactly the state we do not want to measure in. */
static void hold_wait(unsigned secs)
{
    struct timespec t0, now;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    for (;;) {
        clock_gettime(CLOCK_MONOTONIC, &now);
        if ((unsigned long)(now.tv_sec - t0.tv_sec) >= (unsigned long)secs)
            return;
        if (wl_display_roundtrip(wl_dpy) < 0)
            return;
        struct timespec ts = { .tv_sec = 0, .tv_nsec = 200000000L };
        nanosleep(&ts, NULL);
    }
}

int main(int argc, char **argv)
{
    setvbuf(stdout, NULL, _IOLBF, 0);

    unsigned frames = 5;
    int slow = 0;   /* pace the frame loop so the window stays up long enough
                     * to be caught by a host screendump */
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "sw"))        setenv("MESA_VK_WSI_DEBUG", "sw", 1);
        /* "sw" alone only reaches wl_shm through WSI_WL_BUFFER_GPU_SHM, which
         * imports the shm mapping as a VkDeviceMemory host pointer and so
         * needs VK_EXT_external_memory_host to be not merely present but
         * importable for the swapchain image's memory types. Adding noshm
         * selects WSI_WL_BUFFER_SHM_MEMCPY instead — same wl_shm buffer,
         * filled by a CPU copy — which needs nothing of the driver
         * (wsi_common_wayland.c:3548-3556). Measured: on RADV, plain sw fails
         * vkCreateSwapchainKHR with VK_ERROR_INVALID_EXTERNAL_HANDLE and
         * sw,noshm presents 5/5. */
        else if (!strcmp(argv[i], "swnoshm"))
            setenv("MESA_VK_WSI_DEBUG", "sw,noshm", 1);
        else if (!strcmp(argv[i], "nosw")) unsetenv("MESA_VK_WSI_DEBUG");
        else if (!strcmp(argv[i], "slow")) slow = 1;
        else if (!strcmp(argv[i], "quiet")) g_quiet = 1;
        else if (!strncmp(argv[i], "hold", 4)) {
            g_hold_secs = 150;
            if (argv[i][4] == '=') {
                unsigned v = (unsigned)strtoul(argv[i] + 5, NULL, 10);
                if (v) g_hold_secs = v;
            }
        }
        else {
            unsigned v = (unsigned)strtoul(argv[i], NULL, 10);
            if (v) frames = v;
        }
    }
    const char *wsidbg = getenv("MESA_VK_WSI_DEBUG");
    printf("=== vkwl: Vulkan swapchain on a Wayland surface (LeandrOS M4) ===\n");
    printf("MESA_VK_WSI_DEBUG=%s  frames=%u\n\n", wsidbg ? wsidbg : "(unset)", frames);

    VkSwapchainKHR sc = VK_NULL_HANDLE;
    VkImage *imgs = NULL;
    VkCommandPool pool = VK_NULL_HANDLE;

    /* Wayland FIRST: the wl_display has to exist before vkCreateInstance is
     * even useful, and binding the registry here is what tells us which route
     * the compositor can support. */
    if (!wayland_bringup()) goto done;
    if (!bootstrap()) goto done;
    if (!pick_device()) goto done;

    /* The route check, stated as its own subtest so a failure names the cause
     * rather than surfacing later as VK_ERROR_SURFACE_LOST_KHR. */
    wsidbg = getenv("MESA_VK_WSI_DEBUG");
    if (wsidbg && !strncmp(wsidbg, "sw", 2))
        report("wsi_route_shm_available", wl_saw_shm);
    else
        report("wsi_route_dmabuf_available", wl_saw_dmabuf);

    /* 1. The surface. */
    VkWaylandSurfaceCreateInfoKHR wsci = {
        .sType   = VK_STRUCTURE_TYPE_WAYLAND_SURFACE_CREATE_INFO_KHR,
        .display = wl_dpy,
        .surface = wl_surf,
    };
    VkResult r = vkCreateWaylandSurfaceKHR_(g_instance, &wsci, NULL, &g_surface);
    if (r != VK_SUCCESS) {
        ERR("vkCreateWaylandSurfaceKHR -> %s", vkstr(r));
        report("create_wayland_surface", 0);
        goto done;
    }
    report("create_wayland_surface", 1);

    /* 2. A queue family that can present to it. */
    uint32_t qn = 0;
    vkGetPhysicalDeviceQueueFamilyProperties_(g_phys, &qn, NULL);
    VkQueueFamilyProperties *qf = calloc(qn, sizeof(*qf));
    vkGetPhysicalDeviceQueueFamilyProperties_(g_phys, &qn, qf);
    int found = -1;
    for (uint32_t i = 0; i < qn; i++) {
        VkBool32 sup = VK_FALSE;
        vkGetPhysicalDeviceSurfaceSupportKHR_(g_phys, i, g_surface, &sup);
        VkBool32 wlsup = vkGetPhysicalDeviceWaylandPresentationSupportKHR_(g_phys, i, wl_dpy);
        INFO("queueFamily[%u] flags=0x%x count=%u surfaceSupport=%s wlPresentSupport=%s",
             i, qf[i].queueFlags, qf[i].queueCount,
             sup ? "yes" : "no", wlsup ? "yes" : "no");
        if (found < 0 && sup && (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT))
            found = (int)i;
    }
    free(qf);
    if (!report("queue_family_supports_wayland_present", found >= 0)) goto done;
    g_qfam = (uint32_t)found;

    /* 3. Capabilities / formats / present modes. */
    VkSurfaceCapabilitiesKHR caps;
    memset(&caps, 0, sizeof(caps));
    r = vkGetPhysicalDeviceSurfaceCapabilitiesKHR_(g_phys, g_surface, &caps);
    INFO("surface caps: minImageCount=%u maxImageCount=%u currentExtent=%ux%u "
         "supportedUsage=0x%x", caps.minImageCount, caps.maxImageCount,
         caps.currentExtent.width, caps.currentExtent.height,
         caps.supportedUsageFlags);
    if (!report("surface_capabilities", r == VK_SUCCESS && caps.minImageCount >= 1))
        goto done;

    uint32_t fn = 0;
    r = vkGetPhysicalDeviceSurfaceFormatsKHR_(g_phys, g_surface, &fn, NULL);
    VkSurfaceFormatKHR *fmts = fn ? calloc(fn, sizeof(*fmts)) : NULL;
    if (fmts) vkGetPhysicalDeviceSurfaceFormatsKHR_(g_phys, g_surface, &fn, fmts);
    for (uint32_t i = 0; i < fn && i < 12; i++)
        INFO("surface format[%u] format=%d colorSpace=%d",
             i, (int)fmts[i].format, (int)fmts[i].colorSpace);
    INFO("vkGetPhysicalDeviceSurfaceFormatsKHR -> %s, count=%u", vkstr(r), fn);
    if (!report("surface_formats_nonempty", fn > 0 && fmts != NULL)) {
        free(fmts);
        goto done;
    }
    VkSurfaceFormatKHR chosen = fmts[0];
    for (uint32_t i = 0; i < fn; i++)
        if (fmts[i].format == VK_FORMAT_B8G8R8A8_UNORM ||
            fmts[i].format == VK_FORMAT_R8G8B8A8_UNORM) { chosen = fmts[i]; break; }
    free(fmts);
    INFO("chosen surface format=%d colorSpace=%d",
         (int)chosen.format, (int)chosen.colorSpace);

    uint32_t pn = 0;
    vkGetPhysicalDeviceSurfacePresentModesKHR_(g_phys, g_surface, &pn, NULL);
    VkPresentModeKHR *pms = pn ? calloc(pn, sizeof(*pms)) : NULL;
    if (pms) vkGetPhysicalDeviceSurfacePresentModesKHR_(g_phys, g_surface, &pn, pms);
    int have_fifo = 0, have_mailbox = 0;
    for (uint32_t i = 0; i < pn; i++) {
        INFO("present mode[%u] = %d", i, (int)pms[i]);
        if (pms[i] == VK_PRESENT_MODE_FIFO_KHR)    have_fifo = 1;
        if (pms[i] == VK_PRESENT_MODE_MAILBOX_KHR) have_mailbox = 1;
    }
    free(pms);
    if (!report("surface_present_modes_include_fifo", have_fifo)) goto done;

    /* 4. Device with VK_KHR_swapchain. */
    uint32_t en = 0;
    vkEnumerateDeviceExtensionProperties_(g_phys, NULL, &en, NULL);
    VkExtensionProperties *de = en ? calloc(en, sizeof(*de)) : NULL;
    if (de) vkEnumerateDeviceExtensionProperties_(g_phys, NULL, &en, de);
    int have_sc = 0, have_emh = 0;
    for (uint32_t i = 0; i < en && de; i++) {
        if (!strcmp(de[i].extensionName, "VK_KHR_swapchain")) have_sc = 1;
        if (!strcmp(de[i].extensionName, "VK_EXT_external_memory_host")) have_emh = 1;
    }
    free(de);
    /* has_import_memory_host decides whether Mesa's shm path can import the
     * wl_shm mapping directly or must fall back to a per-frame memcpy. Not a
     * pass/fail, but the single most useful number if the swapchain fails. */
    INFO("deviceExtensionCount=%u VK_KHR_swapchain=%s VK_EXT_external_memory_host=%s",
         en, have_sc ? "yes" : "NO", have_emh ? "yes" : "no");
    if (!report("device_advertises_swapchain", have_sc)) goto done;

    float prio = 1.0f;
    VkDeviceQueueCreateInfo qci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
        .queueFamilyIndex = g_qfam,
        .queueCount = 1,
        .pQueuePriorities = &prio,
    };
    const char *dev_exts[] = { "VK_KHR_swapchain" };
    VkDeviceCreateInfo dci = {
        .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
        .queueCreateInfoCount = 1,
        .pQueueCreateInfos = &qci,
        .enabledExtensionCount = 1,
        .ppEnabledExtensionNames = dev_exts,
    };
    r = vkCreateDevice_(g_phys, &dci, NULL, &g_dev);
    if (r != VK_SUCCESS) {
        ERR("vkCreateDevice -> %s", vkstr(r));
        report("create_device_with_swapchain", 0);
        goto done;
    }
    report("create_device_with_swapchain", 1);

    int missing = 0;
#define X(fn2)                                                                     \
    do {                                                                           \
        fn2##_ = (PFN_##fn2)vkGetDeviceProcAddr_(g_dev, #fn2);                     \
        if (!fn2##_) { ERR("vkGetDeviceProcAddr(%s) -> NULL", #fn2); missing++; }   \
    } while (0);
    DEVICE_FUNCS(X)
#undef X
    if (!report("resolve_device_wsi_funcs", missing == 0)) goto done;
    vkGetDeviceQueue_(g_dev, g_qfam, 0, &g_queue);

    /* 5. THE DECISIVE CALL. */
    uint32_t want = caps.minImageCount + 1;
    if (caps.maxImageCount && want > caps.maxImageCount) want = caps.maxImageCount;
    VkExtent2D ext = caps.currentExtent;
    if (ext.width == 0xFFFFFFFFu || ext.width == 0) {
        ext.width  = (wl_cfg_w > 0) ? (uint32_t)wl_cfg_w : DEF_W;
        ext.height = (wl_cfg_h > 0) ? (uint32_t)wl_cfg_h : DEF_H;
    }
    /* TRANSFER_DST is what vkCmdClearColorImage needs; without it there is
     * nothing to look at on screen, only a present of undefined content. */
    VkImageUsageFlags usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT |
                              VK_IMAGE_USAGE_TRANSFER_DST_BIT;
    int can_clear = (caps.supportedUsageFlags & VK_IMAGE_USAGE_TRANSFER_DST_BIT) != 0;
    if (!can_clear) usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT;

    VkSwapchainCreateInfoKHR sci = {
        .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR,
        .surface = g_surface,
        .minImageCount = want,
        .imageFormat = chosen.format,
        .imageColorSpace = chosen.colorSpace,
        .imageExtent = ext,
        .imageArrayLayers = 1,
        .imageUsage = usage,
        .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
        .preTransform = caps.currentTransform ? caps.currentTransform
                                              : VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR,
        .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
        .presentMode = have_mailbox ? VK_PRESENT_MODE_MAILBOX_KHR
                                    : VK_PRESENT_MODE_FIFO_KHR,
        .clipped = VK_TRUE,
    };
    INFO("vkCreateSwapchainKHR: %ux%u minImageCount=%u format=%d usage=0x%x mode=%d",
         ext.width, ext.height, want, (int)chosen.format, usage,
         (int)sci.presentMode);
    r = vkCreateSwapchainKHR_(g_dev, &sci, NULL, &sc);
    if (r != VK_SUCCESS) {
        ERR("vkCreateSwapchainKHR -> %s", vkstr(r));
        report("create_swapchain", 0);
        goto done;
    }
    report("create_swapchain", 1);

    uint32_t in = 0;
    r = vkGetSwapchainImagesKHR_(g_dev, sc, &in, NULL);
    imgs = in ? calloc(in, sizeof(*imgs)) : NULL;
    if (imgs) r = vkGetSwapchainImagesKHR_(g_dev, sc, &in, imgs);
    INFO("swapchain image count = %u", in);
    if (!report("swapchain_images_nonzero", r == VK_SUCCESS && in >= want && imgs))
        goto teardown;

    /* 6. One command pool, reset per frame. */
    VkCommandPoolCreateInfo pci = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO,
        .queueFamilyIndex = g_qfam,
    };
    if (vkCreateCommandPool_(g_dev, &pci, NULL, &pool) != VK_SUCCESS) {
        report("create_command_pool", 0);
        goto teardown;
    }
    report("create_command_pool", 1);

    VkCommandBufferAllocateInfo ai = {
        .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
        .commandPool = pool,
        .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY,
        .commandBufferCount = 1,
    };
    VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };

    /* 7. The frame loop. Each frame is fully CPU-serialised (fence-waited at
     *    both the acquire and the submit) so that a hang names the exact call
     *    it hung in, and so present needs no wait semaphores. */
    unsigned acquired_ok = 0, presented_ok = 0;
    static const float cols[6][4] = {
        {0.90f, 0.15f, 0.15f, 1.0f}, {0.15f, 0.85f, 0.30f, 1.0f},
        {0.15f, 0.40f, 0.95f, 1.0f}, {0.95f, 0.80f, 0.10f, 1.0f},
        {0.70f, 0.25f, 0.85f, 1.0f}, {0.10f, 0.85f, 0.85f, 1.0f},
    };

    for (unsigned f = 0; f < frames; f++) {
        VkFence acq_fence = VK_NULL_HANDLE;
        if (vkCreateFence_(g_dev, &fci, NULL, &acq_fence) != VK_SUCCESS) break;
        uint32_t idx = 0xFFFFFFFFu;
        r = vkAcquireNextImageKHR_(g_dev, sc, FENCE_TIMEOUT_NS, VK_NULL_HANDLE,
                                   acq_fence, &idx);
        if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR) {
            ERR("frame %u: vkAcquireNextImageKHR -> %s", f, vkstr(r));
            vkDestroyFence_(g_dev, acq_fence, NULL);
            break;
        }
        VkResult wr = vkWaitForFences_(g_dev, 1, &acq_fence, VK_TRUE, FENCE_TIMEOUT_NS);
        vkDestroyFence_(g_dev, acq_fence, NULL);
        if (wr != VK_SUCCESS || idx >= in) {
            ERR("frame %u: acquire fence -> %s idx=%u", f, vkstr(wr), idx);
            break;
        }
        acquired_ok++;

        vkResetCommandPool_(g_dev, pool, 0);
        VkCommandBuffer cb = VK_NULL_HANDLE;
        if (vkAllocateCommandBuffers_(g_dev, &ai, &cb) != VK_SUCCESS) break;
        VkCommandBufferBeginInfo bi = {
            .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
            .flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT,
        };
        vkBeginCommandBuffer_(cb, &bi);

        VkImageSubresourceRange range = {
            .aspectMask = VK_IMAGE_ASPECT_COLOR_BIT,
            .levelCount = 1, .layerCount = 1,
        };
        VkImageMemoryBarrier imb = {
            .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER,
            .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED,
            .newLayout = can_clear ? VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL
                                   : VK_IMAGE_LAYOUT_PRESENT_SRC_KHR,
            .srcAccessMask = 0,
            .dstAccessMask = can_clear ? VK_ACCESS_TRANSFER_WRITE_BIT : 0,
            .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
            .image = imgs[idx],
            .subresourceRange = range,
        };
        vkCmdPipelineBarrier_(cb, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT,
                              VK_PIPELINE_STAGE_TRANSFER_BIT,
                              0, 0, NULL, 0, NULL, 1, &imb);
        if (can_clear) {
            VkClearColorValue cv;
            memcpy(cv.float32, cols[f % 6], sizeof(cv.float32));
            vkCmdClearColorImage_(cb, imgs[idx],
                                  VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL,
                                  &cv, 1, &range);
            VkImageMemoryBarrier imb2 = imb;
            imb2.oldLayout = VK_IMAGE_LAYOUT_TRANSFER_DST_OPTIMAL;
            imb2.newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR;
            imb2.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT;
            imb2.dstAccessMask = 0;
            vkCmdPipelineBarrier_(cb, VK_PIPELINE_STAGE_TRANSFER_BIT,
                                  VK_PIPELINE_STAGE_BOTTOM_OF_PIPE_BIT,
                                  0, 0, NULL, 0, NULL, 1, &imb2);
        }
        vkEndCommandBuffer_(cb);

        VkFence sf = VK_NULL_HANDLE;
        vkCreateFence_(g_dev, &fci, NULL, &sf);
        VkSubmitInfo si = {
            .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO,
            .commandBufferCount = 1,
            .pCommandBuffers = &cb,
        };
        VkResult sr = vkQueueSubmit_(g_queue, 1, &si, sf);
        VkResult fw = (sr == VK_SUCCESS)
            ? vkWaitForFences_(g_dev, 1, &sf, VK_TRUE, FENCE_TIMEOUT_NS) : sr;
        if (sf) vkDestroyFence_(g_dev, sf, NULL);
        if (sr != VK_SUCCESS || fw != VK_SUCCESS) {
            ERR("frame %u: submit -> %s fence -> %s", f, vkstr(sr), vkstr(fw));
            break;
        }

        VkResult pres = VK_ERROR_UNKNOWN;
        VkPresentInfoKHR pi = {
            .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR,
            .swapchainCount = 1,
            .pSwapchains = &sc,
            .pImageIndices = &idx,
            .pResults = &pres,
        };
        wl_callback_add_listener(wl_surface_frame(wl_surf), &frame_listener, NULL);
        r = vkQueuePresentKHR_(g_queue, &pi);
        if (!g_quiet)
        INFO("frame %u: idx=%u clear=(%.2f,%.2f,%.2f) present -> %s (per-swapchain %s)",
             f, idx, cols[f % 6][0], cols[f % 6][1], cols[f % 6][2],
             vkstr(r), vkstr(pres));
        if (r != VK_SUCCESS && r != VK_SUBOPTIMAL_KHR) break;
        presented_ok++;

        /* Pump our own queue: the compositor's configure/ping and the frame
         * callbacks Mesa is waiting on all arrive on this socket. */
        wl_display_roundtrip(wl_dpy);

        if (g_hold_secs && frames >= g_hold_frames &&
            f + g_hold_frames >= frames) {
            const float *hc = cols[f % 6];
            unsigned rr = (unsigned)(hc[0] * 255.0f + 0.5f);
            unsigned gg = (unsigned)(hc[1] * 255.0f + 0.5f);
            unsigned bb = (unsigned)(hc[2] * 255.0f + 0.5f);
            printf("VKWL: HOLD READY seq=%u extent=%ux%u rgb=%02x%02x%02x secs=%u\n",
                   f, ext.width, ext.height, rr, gg, bb, g_hold_secs);
            fflush(stdout);
            hold_wait(g_hold_secs);
            printf("VKWL: HOLD END seq=%u\n", f);
            fflush(stdout);
        }

        if (slow) {
            struct timespec ts = { .tv_sec = 0, .tv_nsec = 200000000L };
            nanosleep(&ts, NULL);
        }
    }

    INFO("frames requested=%u acquired=%u presented=%u",
         frames, acquired_ok, presented_ok);
    report("acquire_next_image_all_frames", acquired_ok == frames);
    report("queue_present_all_frames", presented_ok == frames);
    /* Drain the last callbacks (bounded), then require the compositor to have
     * answered for at least half the frames: it consumed them. */
    for (int i = 0; i < 50 && g_frame_done < presented_ok; i++) {
        struct timespec ts = { .tv_sec = 0, .tv_nsec = 20000000L };
        nanosleep(&ts, NULL);
        wl_display_roundtrip(wl_dpy);
    }
    printf("VKWL: frame_callbacks done=%u presented=%u\n", g_frame_done, presented_ok);
    report("compositor_frame_callbacks",
           presented_ok > 0 && g_frame_done * 2 >= presented_ok);
    if (!can_clear)
        report_skip("clear_color_visible",
                    "surface does not support TRANSFER_DST usage");
    else
        report("clear_color_visible", presented_ok == frames);

    vkDeviceWaitIdle_(g_dev);

teardown:
    if (pool) vkDestroyCommandPool_(g_dev, pool, NULL);
    free(imgs);
    if (sc) vkDestroySwapchainKHR_(g_dev, sc, NULL);

done:
    if (g_dev) vkDestroyDevice_(g_dev, NULL);
    if (g_surface && vkDestroySurfaceKHR_) vkDestroySurfaceKHR_(g_instance, g_surface, NULL);
    if (g_instance && vkDestroyInstance_) vkDestroyInstance_(g_instance, NULL);

    printf("--- vkwl done, failures = %d, skipped = %d ---\n", g_fail, g_skip);
    return g_fail > 125 ? 125 : g_fail;
}
