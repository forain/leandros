// nvktest 'triangle' / 'triangle_depth' — fragment shader.
// Copied verbatim from vkrender (leandros-artifacts/venus-lane/shaders/triangle.frag).
//
// Constant opaque red. The attachment is VK_FORMAT_R8G8B8A8_UNORM with
// blending disabled and 1 sample, so 1.0 and 0.0 convert to exactly 255 and 0:
// every covered pixel must read back as the byte sequence FF 00 00 FF, and
// every uncovered pixel as the clear colour 00 00 FF FF.
//
// Regenerate the SPIR-V header with ports/nvk-tests/gen-spv.sh.

#version 450

layout(location = 0) out vec4 o_color;

void main() {
    o_color = vec4(1.0, 0.0, 0.0, 1.0);
}
