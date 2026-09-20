// Consumer: gain staging over the delay line.
#include "dsp/delay.h"

#include <cstdint>

float NormalizeGain(float raw) {
    return dsp::Clamp(raw, 0.0f, 1.0f);
}

// SweepGain nudges the normalized block upward for the envelope test.
float SweepGain(float raw, float step) {
    return NormalizeGain(raw) + step;
}

int main() {
    dsp::DelayLine line(512);
    const float wet = line.Process(NormalizeGain(0.25f));
    return static_cast<int>(SweepGain(wet, 0.125f) * 100.0f);
}
