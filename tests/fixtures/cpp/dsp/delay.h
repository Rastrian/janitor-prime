// Delay line DSP: a fixed-maximum delay buffer with tap mixing.
#include <cstddef>
#include <vector>

namespace dsp {

// A delay line with a fixed maximum length and one half-length tap.
class DelayLine {
public:
    DelayLine(std::size_t max_length);

    // Process writes one sample and returns the wet/dry mix (defined in
    // delay.cpp; documented here because readers start from the header).
    float Process(float sample);

    void Reset() noexcept;

private:
    std::size_t write_pos_;
    std::size_t length_;
    // feedback tap
    float feedback_;
    std::vector<float> buffer_;
};

// MixTaps sums the raw and delayed taps.
float MixTaps(float raw, float delayed);

template <typename T>
T Clamp(T value, T lo, T hi) {
    if (value < lo) {
        return lo;
    }
    return value < hi ? value : hi;
}

}  // namespace dsp
