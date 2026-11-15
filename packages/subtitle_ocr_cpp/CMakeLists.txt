cmake_minimum_required(VERSION 3.10)
project(subtitle_ocr_ort_cpp CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_FLAGS_RELEASE "-O3 -DNDEBUG")
if(NOT CMAKE_BUILD_TYPE)
    set(CMAKE_BUILD_TYPE Release)
endif()

# Output executables to build directory
set(CMAKE_RUNTIME_OUTPUT_DIRECTORY ${CMAKE_BINARY_DIR})

# --- ORT ---------------------------------------------------------------------
set(ORT_DIR "" CACHE PATH "Path to onnxruntime installation (containing include/ and lib/)")
if(NOT ORT_DIR)
    if(WIN32)
        set(ORT_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../tmp/onnxruntime-win-x64-1.26.0/onnxruntime-win-x64-1.26.0")
    else()
        # Auto-detect the ORT copy bundled under packages/tmp/.  Deliberately
        # version-agnostic: we do not hardcode 1.24.4 any more, because the
        # benchmark needs to build against several ORT releases (1.24.4 /
        # 1.28.0 / 1.32.0) and the newest bundled one should win by default.
        # 本仓库源码位于 packages/subtitle_ocr_cpp/（比 LocalDub 的
        # packages/subtitle_ocr/ort-cpp/ 浅一层），故 ../tmp 才是 packages/tmp；
        # 保留 ../../tmp 分支以兼容 LocalDub 那种更深的布局。
        foreach(_ort_root
                "${CMAKE_CURRENT_SOURCE_DIR}/../tmp"
                "${CMAKE_CURRENT_SOURCE_DIR}/../../tmp"
                "/tmp")
            if(ORT_DIR)
                break()
            endif()
            file(GLOB _ort_candidates
                "${_ort_root}/onnxruntime-linux-x64-*")
            if(_ort_candidates)
                # Newest version first; pick the first dir that actually
                # contains include/ + lib/ so a half-extracted tarball is
                # skipped instead of failing the build.
                list(SORT _ort_candidates COMPARE NATURAL ORDER DESCENDING)
                foreach(_cand IN LISTS _ort_candidates)
                    if(EXISTS "${_cand}/include/onnxruntime_cxx_api.h"
                       AND EXISTS "${_cand}/lib/libonnxruntime.so")
                        set(ORT_DIR "${_cand}")
                        break()
                    endif()
                endforeach()
            endif()
        endforeach()
    endif()
endif()
if(NOT ORT_DIR)
    message(FATAL_ERROR
        "onnxruntime not found. Extract it under packages/tmp/onnxruntime-linux-x64-<ver> "
        "or configure with -DORT_DIR=/path/to/onnxruntime")
endif()
message(STATUS "ORT_DIR: ${ORT_DIR}")
set(ORT_INCLUDE_DIR "${ORT_DIR}/include")
set(ORT_LIB_DIR "${ORT_DIR}/lib")

# --- OpenCV ------------------------------------------------------------------
# We explicitly name only the modules we need so that the linker does not
# try to pull in the optional OpenCV modules `opencv_viz` / `opencv_hdf` —
# those need system VTK / HDF5 libraries which are not always available.
#
# Components used:
#   opencv_core         — cv::Mat, cv::Point, cv::Scalar
#   opencv_imgproc      — cv::dilate, cv::findContours, cv::minAreaRect,
#                         cv::boxPoints, cv::fillPoly, cv::mean, resize,
#                         cv::getPerspectiveTransform, cv::warpPerspective,
#                         cv::rotate
#   opencv_imgcodecs    — (optional) imread / imwrite for debugging images

# Force vcpkg-installed OpenCV over MSYS2 (which may be MinGW-built and incompatible with MSVC)
# CMAKE_FIND_USE_[SYSTEM]_ENVIRONMENT_PATH is NOT disabled, but we set OpenCV_DIR as a cache variable
# so find_package uses it with highest priority in Config mode.
if(WIN32 AND EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/../../submodule/vcpkg/installed/x64-windows/share/opencv4")
    set(OpenCV_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../submodule/vcpkg/installed/x64-windows/share/opencv4" CACHE PATH "OpenCV installation directory" FORCE)
endif()

# On MSYS2/MinGW, prefer the MSYS2-installed OpenCV over vcpkg's
# (disabled above — use vcpkg OpenCV which is MSVC-compatible)
# if(WIN32)
#     if(EXISTS "C:/msys64/mingw64/lib/cmake/opencv4")
#         set(OpenCV_DIR "C:/msys64/mingw64/lib/cmake/opencv4")
#     endif()
# endif()
find_package(OpenCV QUIET COMPONENTS core imgproc imgcodecs)
if(NOT OpenCV_FOUND)
    message(FATAL_ERROR "OpenCV (core, imgproc, imgcodecs) not found. Install libopencv-dev or set OpenCV_DIR.")
endif()
message(STATUS "OpenCV version: ${OpenCV_VERSION}")
message(STATUS "OpenCV include dirs: ${OpenCV_INCLUDE_DIRS}")
message(STATUS "OpenCV libs: ${OpenCV_LIBS}")

# --- Build executable --------------------------------------------------------
add_executable(subtitle_ocr_ort_cpp ocr_pipeline.cpp)

target_include_directories(subtitle_ocr_ort_cpp PRIVATE
    "${ORT_INCLUDE_DIR}"
    "${CMAKE_CURRENT_SOURCE_DIR}"
)

target_link_directories(subtitle_ocr_ort_cpp PRIVATE "${ORT_LIB_DIR}")

if(WIN32)
    find_library(ONNXRUNTIME_LIB onnxruntime PATHS "${ORT_LIB_DIR}" NO_DEFAULT_PATH)
    target_link_libraries(subtitle_ocr_ort_cpp PRIVATE ${ONNXRUNTIME_LIB})
    add_custom_command(TARGET subtitle_ocr_ort_cpp POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "${ORT_LIB_DIR}/onnxruntime.dll"
            "$<TARGET_FILE_DIR:subtitle_ocr_ort_cpp>/"
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "${ORT_LIB_DIR}/onnxruntime_providers_shared.dll"
            "$<TARGET_FILE_DIR:subtitle_ocr_ort_cpp>/"
        COMMENT "Copying ORT DLLs"
    )
else()
    find_library(ONNXRUNTIME_LIB onnxruntime PATHS "${ORT_LIB_DIR}" NO_DEFAULT_PATH)
    target_link_libraries(subtitle_ocr_ort_cpp PRIVATE ${ONNXRUNTIME_LIB} pthread)
    # Resolve the real versioned .so (libonnxruntime.so.1.28.0, ...) instead of
    # hardcoding a version, then expose it in the build dir under the stable
    # soname libonnxruntime.so.1 so running the binary next to it works even
    # without LD_LIBRARY_PATH.
    file(GLOB _ort_so_versioned "${ORT_LIB_DIR}/libonnxruntime.so.1.*")
    list(SORT _ort_so_versioned COMPARE NATURAL)
    if(_ort_so_versioned)
        list(GET _ort_so_versioned -1 ORT_SO_REAL)
    else()
        set(ORT_SO_REAL "${ORT_LIB_DIR}/libonnxruntime.so")
    endif()
    message(STATUS "ORT shared library: ${ORT_SO_REAL}")
    add_custom_command(TARGET subtitle_ocr_ort_cpp POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "${ORT_SO_REAL}"
            "$<TARGET_FILE_DIR:subtitle_ocr_ort_cpp>/libonnxruntime.so.1"
        COMMENT "Copying ORT shared library (${ORT_SO_REAL})"
    )
endif()

# Link OpenCV (after ORT to avoid symbol conflicts)
target_link_libraries(subtitle_ocr_ort_cpp PRIVATE ${OpenCV_LIBS})
