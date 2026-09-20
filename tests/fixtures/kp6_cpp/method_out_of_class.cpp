// KP-6-cpp curated snippet: out-of-class method definitions are
// function_definition siblings at file level, qualified names and all.
// DefineA/DefineB are adjacent; DefineC sits behind its doc comment.

class Gain {
public:
    void DefineA(float &sample);
    void DefineB(float &sample);
    void DefineC(float &sample);
};

void Gain::DefineA(float &sample) {
    sample *= 1.0f;
}

void Gain::DefineB(float &sample) {
    sample *= 0.5f;
}

// DefineC is the soft-clip variant, documented like real code.
void Gain::DefineC(float &sample) {
    if (sample > 1.0f) {
        sample = 1.0f;
    }
}
