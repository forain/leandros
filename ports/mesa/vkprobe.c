/* vkprobe - tiny Vulkan smoke test through the Khronos loader (libvulkan.so.1).
 *
 * Lists every physical device the installed ICDs expose, then on the chosen
 * one creates a device, records vkCmdFillBuffer over a host-visible buffer,
 * submits it, waits on a fence and checks every word. One greppable line:
 *   vkprobe PASS dev=<deviceName> api=1.x.y fill ok 16384/16384
 *   vkprobe FAIL <reason> ...
 * Usage: vkprobe [--want <substring of deviceName>] [--list]
 */
#include <stdio.h>
#include <string.h>
#include <vulkan/vulkan.h>

#define WORDS 16384u
#define PAT 0x5EED0F11u

static int fail(const char *why, VkResult r, const char *dev)
{
	printf("vkprobe FAIL %s r=%d dev=%s\n", why, (int)r, dev ? dev : "-");
	return 1;
}

int main(int argc, char **argv)
{
	const char *want = NULL;
	for (int i = 1; i < argc; i++)
		if (!strcmp(argv[i], "--want") && i + 1 < argc) want = argv[++i];
	VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
		.pApplicationName = "vkprobe", .apiVersion = VK_API_VERSION_1_1 };
	VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
	VkInstance inst;
	VkResult r = vkCreateInstance(&ici, NULL, &inst);
	if (r) return fail("vkCreateInstance", r, NULL);
	uint32_t n = 0;
	r = vkEnumeratePhysicalDevices(inst, &n, NULL);
	if (r || !n) return fail("no-physical-device", r, NULL);
	VkPhysicalDevice pds[8];
	if (n > 8) n = 8;
	vkEnumeratePhysicalDevices(inst, &n, pds);
	int pick = -1;
	VkPhysicalDeviceProperties props[8];
	for (uint32_t i = 0; i < n; i++) {
		vkGetPhysicalDeviceProperties(pds[i], &props[i]);
		printf("# gpu%u: %s api=%u.%u.%u type=%d vendor=0x%x device=0x%x\n", i, props[i].deviceName,
		       VK_VERSION_MAJOR(props[i].apiVersion), VK_VERSION_MINOR(props[i].apiVersion),
		       VK_VERSION_PATCH(props[i].apiVersion), props[i].deviceType, props[i].vendorID, props[i].deviceID);
		if (pick < 0 && (!want || strstr(props[i].deviceName, want))) pick = (int)i;
	}
	if (pick < 0) return fail("wanted-device-absent", VK_SUCCESS, want);
	VkPhysicalDevice pd = pds[pick];
	char dev[128];
	snprintf(dev, sizeof dev, "%s", props[pick].deviceName);
	for (char *p = dev; *p; p++) if (*p == ' ') *p = '_';

	uint32_t qn = 0;
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, NULL);
	VkQueueFamilyProperties qf[8];
	if (qn > 8) qn = 8;
	vkGetPhysicalDeviceQueueFamilyProperties(pd, &qn, qf);
	int fam = -1;
	for (uint32_t i = 0; i < qn && fam < 0; i++)
		if (qf[i].queueFlags & (VK_QUEUE_GRAPHICS_BIT | VK_QUEUE_COMPUTE_BIT | VK_QUEUE_TRANSFER_BIT)) fam = (int)i;
	if (fam < 0) return fail("no-queue-family", VK_SUCCESS, dev);
	float prio = 1.0f;
	VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO,
		.queueFamilyIndex = (uint32_t)fam, .queueCount = 1, .pQueuePriorities = &prio };
	VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO,
		.queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
	VkDevice d;
	if ((r = vkCreateDevice(pd, &dci, NULL, &d))) return fail("vkCreateDevice", r, dev);
	VkQueue q;
	vkGetDeviceQueue(d, (uint32_t)fam, 0, &q);

	VkBufferCreateInfo bci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = WORDS * 4,
		.usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
	VkBuffer buf;
	if ((r = vkCreateBuffer(d, &bci, NULL, &buf))) return fail("vkCreateBuffer", r, dev);
	VkMemoryRequirements mr;
	vkGetBufferMemoryRequirements(d, buf, &mr);
	VkPhysicalDeviceMemoryProperties mp;
	vkGetPhysicalDeviceMemoryProperties(pd, &mp);
	uint32_t mt = UINT32_MAX;
	const VkMemoryPropertyFlags need = VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT;
	for (uint32_t i = 0; i < mp.memoryTypeCount && mt == UINT32_MAX; i++)
		if ((mr.memoryTypeBits & (1u << i)) && (mp.memoryTypes[i].propertyFlags & need) == need) mt = i;
	if (mt == UINT32_MAX) return fail("no-host-visible-memory", VK_SUCCESS, dev);
	VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO,
		.allocationSize = mr.size, .memoryTypeIndex = mt };
	VkDeviceMemory mem;
	if ((r = vkAllocateMemory(d, &mai, NULL, &mem))) return fail("vkAllocateMemory", r, dev);
	vkBindBufferMemory(d, buf, mem, 0);
	uint32_t *map;
	if ((r = vkMapMemory(d, mem, 0, WORDS * 4, 0, (void **)&map))) return fail("vkMapMemory", r, dev);
	memset(map, 0, WORDS * 4);

	VkCommandPoolCreateInfo pci = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = (uint32_t)fam };
	VkCommandPool pool;
	if ((r = vkCreateCommandPool(d, &pci, NULL, &pool))) return fail("vkCreateCommandPool", r, dev);
	VkCommandBufferAllocateInfo cai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO,
		.commandPool = pool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
	VkCommandBuffer cb;
	if ((r = vkAllocateCommandBuffers(d, &cai, &cb))) return fail("vkAllocateCommandBuffers", r, dev);
	VkCommandBufferBeginInfo cbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO,
		.flags = VK_COMMAND_BUFFER_USAGE_ONE_TIME_SUBMIT_BIT };
	vkBeginCommandBuffer(cb, &cbi);
	vkCmdFillBuffer(cb, buf, 0, VK_WHOLE_SIZE, PAT);
	VkMemoryBarrier mb = { .sType = VK_STRUCTURE_TYPE_MEMORY_BARRIER,
		.srcAccessMask = VK_ACCESS_TRANSFER_WRITE_BIT, .dstAccessMask = VK_ACCESS_HOST_READ_BIT };
	vkCmdPipelineBarrier(cb, VK_PIPELINE_STAGE_TRANSFER_BIT, VK_PIPELINE_STAGE_HOST_BIT, 0, 1, &mb, 0, NULL, 0, NULL);
	if ((r = vkEndCommandBuffer(cb))) return fail("vkEndCommandBuffer", r, dev);
	VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
	VkFence fence;
	vkCreateFence(d, &fci, NULL, &fence);
	VkSubmitInfo si = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &cb };
	if ((r = vkQueueSubmit(q, 1, &si, fence))) return fail("vkQueueSubmit", r, dev);
	if ((r = vkWaitForFences(d, 1, &fence, VK_TRUE, 5000000000ull))) return fail("vkWaitForFences", r, dev);
	uint32_t good = 0, first_bad = WORDS;
	for (uint32_t i = 0; i < WORDS; i++) {
		if (map[i] == PAT) good++;
		else if (first_bad == WORDS) first_bad = i;
	}
	if (good != WORDS) {
		printf("vkprobe FAIL mismatch %u/%u first_bad=%u val=0x%08x dev=%s\n", good, WORDS, first_bad, map[first_bad], dev);
		return 1;
	}
	printf("vkprobe PASS dev=%s api=%u.%u.%u fill ok %u/%u\n", dev, VK_VERSION_MAJOR(props[pick].apiVersion),
	       VK_VERSION_MINOR(props[pick].apiVersion), VK_VERSION_PATCH(props[pick].apiVersion), good, WORDS);
	vkDestroyFence(d, fence, NULL);
	vkDestroyCommandPool(d, pool, NULL);
	vkUnmapMemory(d, mem);
	vkFreeMemory(d, mem, NULL);
	vkDestroyBuffer(d, buf, NULL);
	vkDestroyDevice(d, NULL);
	vkDestroyInstance(inst, NULL);
	return 0;
}
