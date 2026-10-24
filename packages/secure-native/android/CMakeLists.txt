cmake_minimum_required(VERSION 3.9.0)
project(SecureNative)

set (CMAKE_VERBOSE_MAKEFILE ON)
set (CMAKE_CXX_STANDARD 17)

execute_process(
    COMMAND node -p "require.resolve('uniffi-bindgen-react-native/package.json')"
    OUTPUT_VARIABLE UNIFFI_BINDGEN_PATH
    OUTPUT_STRIP_TRAILING_WHITESPACE
)
get_filename_component(UNIFFI_BINDGEN_PATH "${UNIFFI_BINDGEN_PATH}" DIRECTORY)

include_directories(
    ../cpp
    ../cpp/generated

    ${UNIFFI_BINDGEN_PATH}/cpp/includes
)

add_library(vibeface-secure-native            SHARED
    ../cpp/vibeface-secure-native.cpp
    ../cpp/generated/vibeface_secure_ffi.cpp
    cpp-adapter.cpp
)

set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -O2")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -fexceptions")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -frtti")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -fstack-protector-all")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -Wall")
set(CMAKE_CXX_FLAGS "${CMAKE_CXX_FLAGS} -Werror")

# Set linker flags for 16KB page size alignment (required for Android 15+)
set(CMAKE_SHARED_LINKER_FLAGS "${CMAKE_SHARED_LINKER_FLAGS} -Wl,-z,max-page-size=16384")

cmake_path(
  SET MY_RUST_LIB
  ${CMAKE_SOURCE_DIR}/src/main/jniLibs/${ANDROID_ABI}/libvibeface_secure_ffi.a
  NORMALIZE
)
add_library(my_rust_lib STATIC IMPORTED)
set_target_properties(my_rust_lib PROPERTIES IMPORTED_LOCATION ${MY_RUST_LIB})

find_package(ReactAndroid REQUIRED CONFIG)
find_library(LOGCAT log)
target_link_libraries(vibeface-secure-native ReactAndroid::reactnative)

find_package(fbjni REQUIRED CONFIG)
target_link_libraries(
  vibeface-secure-native
  fbjni::fbjni
  ReactAndroid::jsi
  ${LOGCAT}
  my_rust_lib
)
