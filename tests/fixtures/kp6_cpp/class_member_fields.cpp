// KP-6-cpp curated snippet: class member adjacency inside
// field_declaration_list — the envelope pair, a section comment, then
// the modulation pair. Comments inside class bodies are named siblings
// between the fields, not preprocessor noise to skip.

class Voice {
public:
    float attack = 0.01f;
    float release = 0.10f;
    // modulation targets
    float vibrato = 0.0f;
    float tremolo = 0.0f;
};
