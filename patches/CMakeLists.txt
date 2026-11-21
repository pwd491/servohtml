find_program(PATCH_EXECUTABLE patch)

if(NOT PATCH_EXECUTABLE)
    message(FATAL_ERROR "Required tool 'patch' not found")
endif()

if(DEFINED ENV{CARGO_HOME} AND NOT "$ENV{CARGO_HOME}" STREQUAL "")
    set(_cargo_home "$ENV{CARGO_HOME}")
else()
    set(_cargo_home "$ENV{HOME}/.cargo")
endif()

set(_cargo_registry "${_cargo_home}/registry/src")

function(apply_cargo_patch)
    cmake_parse_arguments(
        PATCH
        ""
        "NAME;CRATE;VERSION;PATCH"
        ""
        ${ARGN}
    )

    foreach(_required NAME CRATE VERSION PATCH)
        if(NOT PATCH_${_required})
            message(FATAL_ERROR
                "apply_cargo_patch: missing argument ${_required}"
            )
        endif()
    endforeach()

    set(_patch_file "${CMAKE_CURRENT_LIST_DIR}/${PATCH_PATCH}")

    if(NOT EXISTS "${_patch_file}")
        message(FATAL_ERROR
            "Patch file not found: ${_patch_file}"
        )
    endif()

    file(GLOB _crate_dirs
        "${_cargo_registry}/*/${PATCH_CRATE}-${PATCH_VERSION}"
    )

    if(NOT _crate_dirs)
        message(FATAL_ERROR
            "Cargo crate not found: ${PATCH_CRATE} ${PATCH_VERSION}"
        )
    endif()

    list(LENGTH _crate_dirs _crate_count)

    if(NOT _crate_count EQUAL 1)
        message(FATAL_ERROR
            "Expected one source directory for ${PATCH_CRATE} "
            "${PATCH_VERSION}, found ${_crate_count}"
        )
    endif()

    list(GET _crate_dirs 0 _crate_dir)

    execute_process(
        COMMAND "${PATCH_EXECUTABLE}"
            --dry-run
            --reverse
            -p1
            -i "${_patch_file}"
        WORKING_DIRECTORY "${_crate_dir}"
        RESULT_VARIABLE _reverse_result
        OUTPUT_QUIET
        ERROR_QUIET
    )

    if(_reverse_result EQUAL 0)
        message(STATUS "Cargo patch already applied: ${PATCH_NAME}")
        return()
    endif()

    execute_process(
        COMMAND "${PATCH_EXECUTABLE}"
            --forward
            --batch
            -p1
            -i "${_patch_file}"
        WORKING_DIRECTORY "${_crate_dir}"
        RESULT_VARIABLE _patch_result
        OUTPUT_VARIABLE _patch_output
        ERROR_VARIABLE _patch_error
    )

    if(NOT _patch_result EQUAL 0)
        message(FATAL_ERROR
            "Failed to apply Cargo patch '${PATCH_NAME}':\n"
            "${_patch_output}\n${_patch_error}"
        )
    endif()

    message(STATUS "Applied Cargo patch: ${PATCH_NAME}")
endfunction()

if(ANDROID)
    apply_cargo_patch(
        NAME    mozjs_sys_android
        CRATE   mozjs_sys
        VERSION 153.7.0-0
        PATCH   android/mozjs_sys/153.7.0-0.patch
    )
endif()