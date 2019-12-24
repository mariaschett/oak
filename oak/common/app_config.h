/*
 * Copyright 2019 The Project Oak Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

#ifndef OAK_COMMON_APP_CONFIG_H_
#define OAK_COMMON_APP_CONFIG_H_

#include <memory>

#include "oak/proto/manager.pb.h"

namespace oak {

// Build a default application configuration with a single Wasm node of the given
// name and contents, accessible via gRPC.
std::unique_ptr<ApplicationConfiguration> DefaultConfig(const std::string& module_bytes);

// Modify application configuration to make logging available.
void AddLoggingToConfig(ApplicationConfiguration* config);

// Modify application configuration to make a storage proxy available.
void AddStorageToConfig(ApplicationConfiguration* config, const std::string& storage_address);

// Modify application configuration to make gRPC to use a specific port.
void AddGrpcPortToConfig(ApplicationConfiguration* config, const int16_t grpc_port);

// Checks whether the given ApplicationConfiguration is valid.
bool ValidApplicationConfig(const ApplicationConfiguration& config);

}  // namespace oak

#endif  // OAK_COMMON_APP_CONFIG_H_
