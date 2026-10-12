# Issue #43/#74: camera pictures on the hub. backend_daemon runs ffmpeg to
# read a camera's stream (RTSP, or HTTP MJPEG from an ESP32-CAM), decode
# its KEY FRAMES (the STM32MP157 has no video decoder: the A7 decodes in
# software) and write them as JPEG -- and, for live video on the hub's
# screen, decode EVERY frame of the small stream and write RAW RGB pixels
# (rawvideo; scale + pad to the picture's exact size): a JPEG of every
# frame cost a whole core (camera.rs, 2026-10-06).
#
# Built lean: only what that needs. Everything else (encoders, other
# formats, filters, devices, network protocols) is left out: less to
# build, a smaller image, less code reachable from what a camera sends.
#
# LICENSE: ffmpeg is LGPL here (no --enable-gpl, no nonfree). Yocto marks
# it "commercial" because of codec patents (H.264/H.265); accepted for
# this project's own hub in run_build.sh (LICENSE_FLAGS_ACCEPTED =
# "commercial_ffmpeg"). Revisit before selling a hub, like the GPLv3 note
# in the wiki's Security-Plan.

PACKAGECONFIG = "avcodec avformat avfilter swscale swresample"

EXTRA_OECONF += " \
    --disable-everything \
    --disable-doc \
    --enable-network \
    --enable-protocol=tcp,udp,rtp,http,file,pipe \
    --enable-demuxer=rtsp,rtp,sdp,h264,hevc,mjpeg,mpjpeg \
    --enable-parser=h264,hevc,mjpeg \
    --enable-decoder=h264,hevc,mjpeg \
    --enable-encoder=mjpeg,rawvideo \
    --enable-muxer=image2,image2pipe,mjpeg,rawvideo \
    --enable-filter=scale,format,null,fps,pad,aformat,anull,atrim,trim,hflip,vflip,transpose \
    --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb \
"
