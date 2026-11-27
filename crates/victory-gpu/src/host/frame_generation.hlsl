// Screen-space motion from completed images. The newest image is never modified.
#define MOTION_ROOT "DescriptorTable(SRV(t0, numDescriptors=2)), DescriptorTable(UAV(u0))"
#define WARP_ROOT "DescriptorTable(SRV(t0, numDescriptors=3)), DescriptorTable(UAV(u1)), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
#define BLOCK 24

Texture2D<float4> latest : register(t0);
Texture2D<float4> earlier : register(t1);
Texture2D<float4> motion : register(t2);
RWTexture2D<float4> vectors : register(u0);
RWTexture2D<float4> result : register(u1);
SamplerState linearClamp : register(s0);

float luma(float3 c) {
    return dot(c, float3(0.2126, 0.7152, 0.0722));
}

static const int2 taps[5] = {
    int2(0, 0), int2(-6, -6), int2(6, -6), int2(-6, 6), int2(6, 6)
};

float errorAt(int2 at, int2 delta, int2 last, float reference[5]) {
    float sum = 0;
    [unroll]
    for (uint i = 0; i < 5; i++) {
        float sample_ = luma(earlier.Load(int3(clamp(at + taps[i] + delta, 0, last), 0)).rgb);
        sum += abs(reference[i] - sample_);
    }
    return sum * 0.2;
}

[RootSignature(MOTION_ROOT)]
[numthreads(8, 8, 1)]
void estimate(uint3 id : SV_DispatchThreadID) {
    uint w, h;
    vectors.GetDimensions(w, h);
    if (id.x >= w || id.y >= h) return;

    uint pw, ph;
    latest.GetDimensions(pw, ph);
    int2 last = int2(pw, ph) - 1;
    int2 at = min(int2(id.xy) * BLOCK + BLOCK / 2, last);
    float reference[5];
    [unroll]
    for (uint i = 0; i < 5; i++) {
        reference[i] = luma(latest.Load(int3(clamp(at + taps[i], 0, last), 0)).rgb);
    }

    float still = errorAt(at, int2(0, 0), last, reference);
    if (still < 0.012) {
        vectors[id.xy] = 0;
        return;
    }

    int2 best = int2(0, 0);
    float score = still;
    // Hierarchical search over the previous image, then two-pixel refinement.
    [loop]
    for (int step = 16; step >= 2; step /= 2) {
        int2 origin = best;
        [unroll]
        for (int y = -1; y <= 1; y++) {
            [unroll]
            for (int x = -1; x <= 1; x++) {
                int2 candidate = origin + int2(x, y) * step;
                float e = errorAt(at, candidate, last, reference)
                    + 0.00035 * (abs(candidate.x) + abs(candidate.y));
                if (e < score) {
                    score = e;
                    best = candidate;
                }
            }
        }
    }

    float confidence = saturate((still - score - 0.01) * 14.0)
        * (1.0 - smoothstep(0.025, 0.09, score));
    vectors[id.xy] = float4(float2(best), confidence, 0);
}

[RootSignature(WARP_ROOT)]
[numthreads(8, 8, 1)]
void extrapolate(uint3 id : SV_DispatchThreadID) {
    uint width, height;
    result.GetDimensions(width, height);
    if (id.x >= width || id.y >= height) return;

    float2 inverse = 1.0 / float2(width, height);
    float2 uv = (float2(id.xy) + 0.5) * inverse;
    float4 original = latest.Load(int3(id.xy, 0));
    float4 flow = motion.Load(int3(id.xy / BLOCK, 0));
    if (flow.z < 0.45) {
        result[id.xy] = original;
        return;
    }

    uint blocksX, blocksY;
    motion.GetDimensions(blocksX, blocksY);
    float2 flowUV = (float2(id.xy) + 0.5) / (float2(blocksX, blocksY) * BLOCK);
    float4 adjacent = motion.SampleLevel(linearClamp, flowUV, 0);
    if (any(abs(adjacent.xy - flow.xy) > 2.0) || adjacent.z < 0.4) {
        // Motion disagrees across this boundary. Keep the real image sharp.
        result[id.xy] = original;
        return;
    }

    float2 sourceUV = uv + flow.xy * inverse * 0.5;
    float2 oldUV = sourceUV + flow.xy * inverse;
    if (any(oldUV < 0.0) || any(oldUV > 1.0)) {
        result[id.xy] = original;
        return;
    }

    float4 shifted = latest.SampleLevel(linearClamp, sourceUV, 0);
    float3 old = earlier.SampleLevel(linearClamp, oldUV, 0).rgb;
    float3 mismatch = abs(old - shifted.rgb);
    // A poor match can be a newly visible surface. Never blend two positions:
    // that leaves a translucent duplicate around moving objects and text.
    result[id.xy] = max(max(mismatch.r, mismatch.g), mismatch.b) <= 0.075 ? shifted : original;
}
