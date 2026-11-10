// The app's window: Qt's side of src/ffi.rs, where each function is described.
#pragma once

#include "rust/cxx.h"

#include <cstdint>

struct Options;

int run(const Options &options);
void show_error(rust::Str appId, rust::Str message);

// Any thread.
void load(rust::Str url);
void raise(rust::Str activationToken);
void bridge_reply(std::uint64_t id, rust::Slice<const std::uint8_t> body);
void quit();
