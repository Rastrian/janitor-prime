// KP-6-cpp curated snippet: namespace-body function adjacency — the
// nested namespace body carries its function_definitions directly, no
// intermediate list node. RouteA/RouteB are adjacent; RouteC sits
// behind its doc comment.

namespace audio {
namespace router {

float RouteA(float s) {
    return s;
}

float RouteB(float s) {
    return s * 0.5f;
}

// RouteC is the attenuated fallback path.
float RouteC(float s) {
    return s * 0.25f;
}

}  // namespace router
}  // namespace audio
