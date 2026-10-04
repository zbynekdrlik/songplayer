// SP-program-MAX compositor (#223 S1a): one textured quad per picture,
// drawn over the black 3840x2160 render target with additive blending.
//
// The constants are sp-gpu's QuadConstants (quad.rs): five float4
// registers in this order. Change both together.

cbuffer Quad : register(b0)
{
    float4 rect;    // the quad in NDC: left, top, right, bottom
    float4 to_r;    // R = dot(to_r, float4(y, u, v, 1))
    float4 to_g;    // G
    float4 to_b;    // B
    float4 weight;  // x: the blend weight
};

Texture2D<float> luma_plane : register(t0);
Texture2D<float2> chroma_plane : register(t1);
SamplerState bilinear : register(s0);

struct Corner
{
    float4 position : SV_Position;
    float2 uv : TEXCOORD0;
};

// A 4-vertex triangle strip, no vertex buffer: vertex i is corner
// (i & 1, i >> 1) of the unit square, placed on the quad's edges exactly.
Corner vs_main(uint id : SV_VertexID)
{
    float2 t = float2((float)(id & 1u), (float)(id >> 1u));
    Corner corner;
    corner.position = float4(t.x > 0.5 ? rect.z : rect.x, t.y > 0.5 ? rect.w : rect.y, 0.0, 1.0);
    corner.uv = t;
    return corner;
}

// Both planes sampled bilinear at the pixel centre's uv (the chroma plane
// on its own half-size grid), BT.709 limited -> full range, saturated,
// times the blend weight. Alpha is not written (the blend state's write
// mask), so it stays the clear's 1.
float4 ps_main(Corner corner) : SV_Target
{
    float y = luma_plane.Sample(bilinear, corner.uv);
    float2 uv = chroma_plane.Sample(bilinear, corner.uv);
    float4 yuv1 = float4(y, uv.x, uv.y, 1.0);
    float3 rgb = saturate(float3(dot(to_r, yuv1), dot(to_g, yuv1), dot(to_b, yuv1)));
    return float4(rgb * weight.x, 1.0);
}
