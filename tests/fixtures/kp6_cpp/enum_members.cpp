// KP-6-cpp curated snippet: enumerator adjacency — enum-class members
// are enumerator siblings inside enumerator_list; a section comment
// splits the format pair from the transport pair.

enum class Codec {
    kPcm,
    kFloat,
    // transport
    kRtsp,
    kSrt,
};
