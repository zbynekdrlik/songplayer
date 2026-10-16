// The UYVY pass (#279 P1a): SP-program's full-range BGRA target -> BT.709
// limited-range UYVY, for NDI. One full-target triangle over an
// R8G8B8A8_UNORM texture of width/2 x height texels: texel (x, y) is the
// pixel pair (2x, y), (2x + 1, y) of the source, written as
// (U, Y0, V, Y1), so its four bytes in memory are one UYVY pair.
//
// The constants are sp-gpu's uyvy::constants_bytes (uyvy.rs): three
// float4 registers in this order, the rows of uyvy::rows_f32. Change
// both together; uyvy.rs's pack_pair is the CPU model of ps_main.

cbuffer Uyvy : register(b0)
{
    float4 to_y; // Y' = dot(to_y, float4(r, g, b, 1)), code / 255
    float4 to_u; // Cb
    float4 to_v; // Cr
};

Texture2D<float4> source_bgra : register(t0);

// Vertex i of one triangle that covers the whole target, no vertex buffer:
// (-1, 1), (3, 1), (-1, -3) in NDC.
float4 vs_main(uint id : SV_VertexID) : SV_Position
{
    float2 t = float2((float)((id << 1u) & 2u), (float)(id & 2u));
    return float4(t.x * 2.0 - 1.0, 1.0 - t.y * 2.0, 0.0, 1.0);
}

// Each source pixel read exactly (Load: no filter), its own Y', and the
// pair's chroma from the two pixels' average RGB. The render target
// saturates and rounds each to its byte.
float4 ps_main(float4 position : SV_Position) : SV_Target
{
    int2 texel = int2(position.xy);
    float3 left = source_bgra.Load(int3(2 * texel.x, texel.y, 0)).rgb;
    float3 right = source_bgra.Load(int3(2 * texel.x + 1, texel.y, 0)).rgb;
    float4 mean = float4((left + right) * 0.5, 1.0);
    return saturate(float4(dot(to_u, mean), dot(to_y, float4(left, 1.0)), dot(to_v, mean), dot(to_y, float4(right, 1.0))));
}
