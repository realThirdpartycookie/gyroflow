// emscripten: the few MDK names VideoTextureNode.cpp/MDKPlayer.h touch; playback is done by MDKPlayer_web.cpp.
#pragma once
namespace mdk {
struct GLRenderAPI { unsigned fbo = 0; };
struct MediaInfo {};
class Player { public: void setRenderAPI(void *) {} };
typedef int LogLevel;
inline void SetGlobalOption(const char *, float) {}
inline void SetGlobalOption(const char *, const char *) {}
template<class F> inline void setLogHandler(F &&) {}
}
