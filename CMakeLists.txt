cmake_minimum_required(VERSION 3.22)

# --- поддерживаемые платформы ---
set(_csweb_ok ON)
if(ANDROID)
    if(NOT ANDROID_ABI MATCHES "^(arm64-v8a|armeabi-v7a)$")
        set(_csweb_ok OFF)
    endif()
elseif(CMAKE_SIZEOF_VOID_P EQUAL 4)
    set(_csweb_ok OFF)
endif()

if(NOT _csweb_ok)
    message(WARNING "CSWeb is not supported on this target, skipping")
    return()
endif()

# --- Rust ---
option(CSWEB_DEBUG_BUILD "Build CSWeb in dev profile" OFF)
set(_profile release)
if(CSWEB_DEBUG_BUILD)
    set(_profile dev)
endif()

include(FetchContent)
FetchContent_Declare(Corrosion
    GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
    GIT_TAG        v0.6.1)
FetchContent_MakeAvailable(Corrosion)

corrosion_import_crate(
    MANIFEST_PATH "${CMAKE_CURRENT_SOURCE_DIR}/rust/Cargo.toml"
    CRATES csweb
    PROFILE ${_profile})
# создаются: csweb (interface), csweb-shared (cdylib)

# --- имя файла как у остальных библиотек проекта ---
set(_prefix "")
if(ANDROID)
    set(_prefix "lib")
endif()
set(_file "${_prefix}csweb${POSTFIX}${CMAKE_SHARED_LIBRARY_SUFFIX}")

# куда класть в build-дереве
if(ANDROID AND CMAKE_LIBRARY_OUTPUT_DIRECTORY)
    set(_out "${CMAKE_LIBRARY_OUTPUT_DIRECTORY}")   # AGP сам упакует .so из этой папки
else()
    set(_out "${CMAKE_BINARY_DIR}/cl_dll")
endif()

add_custom_target(csweb_dist ALL
    COMMAND ${CMAKE_COMMAND} -E make_directory "${_out}"
    COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "$<TARGET_FILE:csweb-shared>" "${_out}/${_file}"
    VERBATIM)
add_dependencies(csweb_dist csweb-shared)

install(FILES "$<TARGET_FILE:csweb-shared>"
        RENAME      "${_file}"
        DESTINATION "${GAME_DIR}/${CLIENT_INSTALL_DIR}"
        PERMISSIONS OWNER_READ OWNER_WRITE OWNER_EXECUTE
                    GROUP_READ GROUP_EXECUTE WORLD_READ WORLD_EXECUTE)

# --- единственный интерфейс для клиента ---
add_library(csweb_client INTERFACE)
add_library(csweb::client ALIAS csweb_client)
target_include_directories(csweb_client INTERFACE "${CMAKE_CURRENT_SOURCE_DIR}/include")
target_compile_definitions(csweb_client INTERFACE
    _CSWEB_ENABLE=1
    CSWEB_LIB_FILE="${_file}")