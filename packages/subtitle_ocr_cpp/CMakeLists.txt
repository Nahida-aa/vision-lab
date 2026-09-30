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
        # Prefer the ORT copy bundled with this project under packages/tmp/.
        # 本仓库源码位于 packages/subtitle_ocr_cpp/（比 LocalDub 的
        # packages/subtitle_ocr/ort-cpp/ 浅一层），故 ../tmp 才是 packages/tmp；
        # 保留 ../../tmp 分支以兼容 LocalDub 那种更深的布局。
        if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/../tmp/onnxruntime-linux-x64-1.24.4")
            set(ORT_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../tmp/onnxruntime-linux-x64-1.24.4")
        elseif(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/../../tmp/onnxruntime-linux-x64-1.24.4")
            set(ORT_DIR "${CMAKE_CURRENT_SOURCE_DIR}/../../tmp/onnxruntime-linux-x64-1.24.4")
        elseif(EXISTS "/tmp/onnxruntime-linux-x64-1.24.4")
            set(ORT_DIR "/tmp/onnxruntime-linux-x64-1.24.4")
        endif()
    endif()
endif()
if(NOT ORT_DIR)
    message(FATAL_ERROR
        "onnxruntime not found. Place it under packages/tmp/onnxruntime-linux-x64-1.24.4 "
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
    add_custom_command(TARGET subtitle_ocr_ort_cpp POST_BUILD
        COMMAND ${CMAKE_COMMAND} -E copy_if_different
            "${ORT_LIB_DIR}/libonnxruntime.so.1.24.4"
            "$<TARGET_FILE_DIR:subtitle_ocr_ort_cpp>/libonnxruntime.so.1"
        COMMENT "Copying ORT shared library"
    )
endif()

# Link OpenCV (after ORT to avoid symbol conflicts)
target_link_libraries(subtitle_ocr_ort_cpp PRIVATE ${OpenCV_LIBS})
