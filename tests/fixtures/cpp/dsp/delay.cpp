// DelayLine implementation: the out-of-class definitions.
#include "delay.h"

DelayLine::DelayLine(std::size_t max_length)
    : write_pos_(0), length_(max_length), feedback_(0.25f), buffer_(max_length, 0.0f) {}

float DelayLine::Process(float sample) {
    const std::size_t tap = (write_pos_ + length_ / 2) % length_;
    buffer_[write_pos_] = sample;
    const float delayed = buffer_[tap];
    write_pos_ = (write_pos_ + 1) % length_;
    return MixTaps(sample, delayed * feedback_);
}

void DelayLine::Reset() noexcept {
    for (float &slot : buffer_) {
        slot = 0.0f;
    }
    write_pos_ = 0;
}

// MixTaps is the dry/wet funnel both taps pass through.
float dsp::MixTaps(float raw, float delayed) {
    return raw + delayed;
}
