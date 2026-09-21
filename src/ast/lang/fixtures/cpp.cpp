// Registry fixture for the cpp grammar (kind-table seeding, E5): a
// plugin/audio-shaped C++ module exercising the kind vocabulary a real
// corpus hits — nested namespaces, a class with in-class methods and
// out-of-class definitions, templates, lambdas, override, using-decls,
// raw strings and both comment forms. Must parse CLEAN (zero ERROR /
// MISSING); fenced by `bundled_fixtures_parse_clean_under_their_grammars`.

#include <cstdint>
#include <cstddef>
#include <string>
#include <vector>

namespace audio {
namespace dsp {

enum class Stage { kIdle, kRunning };

struct Parameters {
    float gain = 1.0f;
    std::size_t window = 512;
};

/* A delay line with a fixed maximum length. Block comments are the same
   `comment` kind as the line comments above (C-family discipline). */
class DelayLine {
public:
    DelayLine(std::size_t max_length)
        : buffer_(max_length, 0.0f), write_pos_(0) {}

    float process(float sample);
    void reset() noexcept;

private:
    std::vector<float> buffer_;
    std::size_t write_pos_;
};

template <typename Sample>
Sample clamp(Sample value, Sample lo, Sample hi) {
    if (value < lo) {
        return lo;
    }
    return value < hi ? value : hi;
}

float DelayLine::process(float sample) {
    buffer_[write_pos_] = sample;
    const float delayed = buffer_[(write_pos_ + buffer_.size() / 2) % buffer_.size()];
    write_pos_ = (write_pos_ + 1) % buffer_.size();
    return sample + delayed;
}

void DelayLine::reset() noexcept {
    for (auto &slot : buffer_) {
        slot = 0.0f;
    }
    write_pos_ = 0;
}

float mix(const std::vector<float> &channels, const Parameters &params) {
    auto sum = 0.0f;
    for (float channel : channels) {
        sum += channel * params.gain;
    }
    return sum;
}

class Processor {
public:
    virtual ~Processor() = default;
    virtual float process(float sample) = 0;
};

class BypassProcessor final : public Processor {
public:
    float process(float sample) override { return sample; }
};

}  // namespace dsp
}  // namespace audio
