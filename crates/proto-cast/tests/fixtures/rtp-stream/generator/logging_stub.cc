// A minimal embedder for openscreen's logging API.
//
// openscreen's own platform/impl/logging_posix.cc pulls in Chromium's build/
// repository for build_config.h, which is a gclient dependency we deliberately do not
// fetch. The logging API is only three functions and the fixture generator has no use
// for log output beyond seeing fatal errors, so implementing it directly is cheaper
// and more reproducible than dragging in the Chromium build tree.

#include "platform/api/logging.h"

#include <cstdio>
#include <cstdlib>

namespace openscreen {

bool IsLoggingOn(LogLevel level, const std::string_view file) {
  return level >= LogLevel::kError;
}

void LogWithLevel(LogLevel level,
                  const char* file,
                  int line,
                  std::stringstream message) {
  fprintf(stderr, "[%s:%d] %s\n", file, line, message.str().c_str());
}

[[noreturn]] void Break() {
  abort();
}

}  // namespace openscreen
