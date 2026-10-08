// nvktest 'triangle' / 'triangle_depth' — vertex shader.
// Copied from vkrender (leandros-artifacts/venus-lane/shaders/triangle.vert),
// the Venus-lane GPU submission test; geometry and analytic area unchanged.
//
// No vertex buffers, no vertex input state: the three positions come from
// gl_VertexIndex, so the draw is `vkCmdDraw(cb, 3, 1, 0, 0)` with an empty
// VkPipelineVertexInputStateCreateInfo.
//
// NDC positions and the framebuffer pixels they land on for a 256x256
// viewport (screen_x = (ndc.x*0.5+0.5)*256, screen_y = (ndc.y*0.5+0.5)*256;
// Vulkan NDC has y pointing DOWN, so -0.75 is near the TOP of the image):
//
//   index 0: ( 0.00, -0.75) -> (128,  32)   apex, top-centre
//   index 1: ( 0.75,  0.75) -> (224, 224)   bottom-right
//   index 2: (-0.75,  0.75) -> ( 32, 224)   bottom-left
//
// Analytic area = 0.5 * base * height = 0.5 * 192 * 192 = 18432 pixels.
//
// z = 0.5: with the depth variant (clear depth 1.0, compare LESS) every
// fragment passes, so both variants must produce the same image.
// (vkrender used z = 0.0; without a depth attachment z is irrelevant.)
//
// Culling is disabled in the pipeline (VK_CULL_MODE_NONE), so the winding
// order of the three vertices above is deliberately not load-bearing.
//
// Regenerate the SPIR-V header with ports/nvk-tests/gen-spv.sh.

#version 450

void main() {
    vec2 p;
    if (gl_VertexIndex == 0) {
        p = vec2(0.0, -0.75);
    } else if (gl_VertexIndex == 1) {
        p = vec2(0.75, 0.75);
    } else {
        p = vec2(-0.75, 0.75);
    }
    gl_Position = vec4(p, 0.5, 1.0);
}
