// evrace: does MoltenVK's VkEvent (MVKEventNative) encode a GPU wait for a
// value nobody will signal? Per iteration: cmd A = fill + vkCmdSetEvent(E),
// cmd B = vkCmdWaitEvents(E) + fill, submitted back to back on one queue.
// If B's fence is not done 250 ms after A's, B is stuck: we read E's
// MTLSharedEvent (VK_EXT_metal_objects), record the value, and release the
// GPU wait from the host before Metal's ~5 s watchdog fires.
#import <Metal/Metal.h>
#define VK_USE_PLATFORM_METAL_EXT
#include <vulkan/vulkan.h>
#include <vulkan/vulkan_metal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#define CK(x) do { VkResult r_ = (x); if (r_ != VK_SUCCESS) { fprintf(stderr, "%s = %d\n", #x, r_); exit(1); } } while (0)
static double now(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC, &t); return t.tv_sec + t.tv_nsec * 1e-9; }
int main(int argc, char **argv) {
    long iters = argc > 1 ? atol(argv[1]) : 20000;
    VkApplicationInfo ai = { VK_STRUCTURE_TYPE_APPLICATION_INFO, 0, "evrace", 1, 0, 0, VK_API_VERSION_1_2 };
    const char *iext[] = { "VK_KHR_portability_enumeration", "VK_KHR_get_physical_device_properties2" };
    VkInstanceCreateInfo ici = { VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, 0, VK_INSTANCE_CREATE_ENUMERATE_PORTABILITY_BIT_KHR, &ai, 0, 0, 2, iext };
    VkInstance inst; CK(vkCreateInstance(&ici, 0, &inst));
    uint32_t n = 1; VkPhysicalDevice pd; vkEnumeratePhysicalDevices(inst, &n, &pd);
    float prio = 1; VkDeviceQueueCreateInfo qci = { VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, 0, 0, 0, 1, &prio };
    const char *dext[] = { "VK_KHR_portability_subset", "VK_EXT_metal_objects" };
    VkDeviceCreateInfo dci = { VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, 0, 0, 1, &qci, 0, 0, 2, dext, 0 };
    VkDevice dev; CK(vkCreateDevice(pd, &dci, 0, &dev));
    PFN_vkExportMetalObjectsEXT exportMO = (PFN_vkExportMetalObjectsEXT)vkGetDeviceProcAddr(dev, "vkExportMetalObjectsEXT");
    VkQueue q; vkGetDeviceQueue(dev, 0, 0, &q);
    VkDeviceSize sz = 8 << 20;
    VkBufferCreateInfo bci = { VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, 0, 0, sz, VK_BUFFER_USAGE_TRANSFER_DST_BIT, 0, 0, 0 };
    VkBuffer buf; CK(vkCreateBuffer(dev, &bci, 0, &buf));
    VkMemoryRequirements mr; vkGetBufferMemoryRequirements(dev, buf, &mr);
    VkPhysicalDeviceMemoryProperties mp; vkGetPhysicalDeviceMemoryProperties(pd, &mp);
    uint32_t mt = 0; for (; mt < mp.memoryTypeCount; mt++) if (mr.memoryTypeBits & (1u << mt)) break;
    VkMemoryAllocateInfo mai = { VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, 0, mr.size, mt };
    VkDeviceMemory mem; CK(vkAllocateMemory(dev, &mai, 0, &mem)); CK(vkBindBufferMemory(dev, buf, mem, 0));
    VkCommandPoolCreateInfo cpi = { VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, 0, VK_COMMAND_POOL_CREATE_RESET_COMMAND_BUFFER_BIT, 0 };
    VkCommandPool pool; CK(vkCreateCommandPool(dev, &cpi, 0, &pool));
    VkCommandBufferAllocateInfo cai = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, 0, pool, VK_COMMAND_BUFFER_LEVEL_PRIMARY, 2 };
    VkCommandBuffer cb[2]; CK(vkAllocateCommandBuffers(dev, &cai, cb));
    VkFenceCreateInfo fci = { VK_STRUCTURE_TYPE_FENCE_CREATE_INFO, 0, 0 };
    VkFence fa, fb; CK(vkCreateFence(dev, &fci, 0, &fa)); CK(vkCreateFence(dev, &fci, 0, &fb));
    VkCommandBufferBeginInfo bi = { VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO, 0, VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT, 0 };
    long stuck = 0; double t0 = now();
    for (long i = 0; i < iters; i++) {
        VkEventCreateInfo eci = { VK_STRUCTURE_TYPE_EVENT_CREATE_INFO, 0, 0 };
        VkEvent ev; CK(vkCreateEvent(dev, &eci, 0, &ev));
        VkDeviceSize fill = (VkDeviceSize)(4096 << (i % 10)); // vary A's GPU time
        CK(vkBeginCommandBuffer(cb[0], &bi));
        vkCmdFillBuffer(cb[0], buf, 0, fill, (uint32_t)i);
        vkCmdSetEvent(cb[0], ev, VK_PIPELINE_STAGE_TRANSFER_BIT);
        CK(vkEndCommandBuffer(cb[0]));
        CK(vkBeginCommandBuffer(cb[1], &bi));
        vkCmdWaitEvents(cb[1], 1, &ev, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, 0, 0, 0, 0);
        vkCmdFillBuffer(cb[1], buf, sz / 2, 4096, 7);
        CK(vkEndCommandBuffer(cb[1]));
        VkSubmitInfo sa = { VK_STRUCTURE_TYPE_SUBMIT_INFO, 0, 0, 0, 0, 1, &cb[0], 0, 0 };
        VkSubmitInfo sb = { VK_STRUCTURE_TYPE_SUBMIT_INFO, 0, 0, 0, 0, 1, &cb[1], 0, 0 };
        CK(vkQueueSubmit(q, 1, &sa, fa));
        { // jitter: sweep 0..400 us so A's GPU-side signal lands anywhere around B's encode
            double until = now() + (double)((i * 7919) % 400) * 1e-6;
            while (now() < until) {}
        }
        CK(vkQueueSubmit(q, 1, &sb, fb));
        CK(vkWaitForFences(dev, 1, &fa, VK_TRUE, 2000000000ull));
        if (vkWaitForFences(dev, 1, &fb, VK_TRUE, 250000000ull) == VK_TIMEOUT) {
            VkExportMetalSharedEventInfoEXT se = { VK_STRUCTURE_TYPE_EXPORT_METAL_SHARED_EVENT_INFO_EXT, 0, VK_NULL_HANDLE, ev, nil };
            VkExportMetalObjectsInfoEXT eo = { VK_STRUCTURE_TYPE_EXPORT_METAL_OBJECTS_INFO_EXT, &se };
            exportMO(dev, &eo);
            id<MTLSharedEvent> me = (id<MTLSharedEvent>)se.mtlSharedEvent;
            uint64_t v = me.signaledValue;
            // Slow or suspended rather than deadlocked? Give it 1.5 s more
            // (still well under Metal's ~5 s watchdog) before calling it.
            if (vkWaitForFences(dev, 1, &fb, VK_TRUE, 1500000000ull) == VK_SUCCESS) {
                printf("iter %ld: B late but completed by itself (signaledValue=%llu)\n", i, (unsigned long long)v);
                vkResetFences(dev, 1, &fa); vkResetFences(dev, 1, &fb); vkDestroyEvent(dev, ev, 0);
                continue;
            }
            v = me.signaledValue;
            stuck++;
            printf("iter %ld: DEADLOCK: B still waiting 1.75 s after A completed; event signaledValue=%llu -> releasing from host\n", i, (unsigned long long)v);
            me.signaledValue = v + 16;   // release the GPU wait (any value >= the awaited one)
            CK(vkWaitForFences(dev, 1, &fb, VK_TRUE, 2000000000ull));
        }
        vkResetFences(dev, 1, &fa); vkResetFences(dev, 1, &fb);
        vkDestroyEvent(dev, ev, 0);
        if ((i + 1) % 5000 == 0) { printf("%ld iters, %ld stuck, %.1f s\n", i + 1, stuck, now() - t0); fflush(stdout); }
    }
    printf("DONE iters=%ld stuck=%ld\n", iters, stuck);
    return 0;
}
