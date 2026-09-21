// KP-6-cpp curated snippet: local declaration adjacency inside a
// compound_statement — the C-family statement-level idiom. base/gain
// are adjacent; shaped sits behind the stage comment.

float Shape(float raw) {
    float base = raw * 0.5f;
    float gain = 2.0f;
    // shaping follows the gain stage
    float shaped = base * gain;
    return shaped;
}
