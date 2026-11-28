// Minimal ABI accessor shim for runtime-loaded FFmpeg (libavcodec 60/61/62 + libavutil 58/59/60).
//
// Struct prefixes are verified against upstream FFmpeg headers (n6.1.2, n7.1.1, n8.0) so the C
// compiler computes exact alignment and padding for both x86_64 and aarch64 without requiring
// system FFmpeg -dev headers at build time. Non-AVOption AVCodecContext fields (hw_frames_ctx,
// framerate) are additionally anchored and checked against runtime av_opt_find() offsets.

#include <stddef.h>
#include <stdint.h>

typedef struct {
    int num;
    int den;
} AhRational;

// AVBufferRef (libavutil 58, 59, 60)
typedef struct {
    void *buffer;
    uint8_t *data;
    size_t size;
} AhBufferRef;

// AVPacket prefix (libavcodec 60, 61, 62)
typedef struct {
    void *buf;
    int64_t pts;
    int64_t dts;
    uint8_t *data;
    int size;
    int stream_index;
    int flags;
} AhPacket;

// AVHWFramesContext (libavutil 58: includes `internal`)
typedef struct {
    const void *av_class;
    void *internal;
    void *device_ref;
    void *device_ctx;
    void *hwctx;
    void (*free_cb)(void *);
    void *user_opaque;
    void *pool;
    int initial_pool_size;
    int format;
    int sw_format;
    int width;
    int height;
} AhHWFramesContext58;

// AVHWFramesContext (libavutil 59, 60: `internal` moved to FFHWFramesContext)
typedef struct {
    const void *av_class;
    void *device_ref;
    void *device_ctx;
    void *hwctx;
    void (*free_cb)(void *);
    void *user_opaque;
    void *pool;
    int initial_pool_size;
    int format;
    int sw_format;
    int width;
    int height;
} AhHWFramesContext59;

// AVFrame prefix (libavutil 58, 59: includes deprecated `key_frame`)
typedef struct {
    uint8_t *data[8];
    int linesize[8];
    uint8_t **extended_data;
    int width;
    int height;
    int nb_samples;
    int format;
    int key_frame;
    int pict_type;
    AhRational sample_aspect_ratio;
    int64_t pts;
} AhFrame58;

// AVFrame prefix (libavutil 60: `key_frame` removed)
typedef struct {
    uint8_t *data[8];
    int linesize[8];
    uint8_t **extended_data;
    int width;
    int height;
    int nb_samples;
    int format;
    int pict_type;
    AhRational sample_aspect_ratio;
    int64_t pts;
} AhFrame60;

// AVCodecContext hw_frames_ctx neighborhood (libavcodec 60)
typedef struct {
    void *hw_frames_ctx;
    int trailing_padding;
    int64_t max_pixels;
    void *hw_device_ctx;
    int hwaccel_flags;
} AhCtxHw60;

// AVCodecContext hw_frames_ctx neighborhood (libavcodec 61, 62)
typedef struct {
    void *hw_frames_ctx;
    void *hw_device_ctx;
    int hwaccel_flags;
    int extra_hw_frames;
} AhCtxHw61;

// AVCodecContext framerate neighborhood (libavcodec 60)
typedef struct {
    AhRational framerate;
    int sw_pix_fmt;
    AhRational pkt_timebase;
} AhCtxFps60;

// AVCodecContext framerate neighborhood (libavcodec 61, 62)
typedef struct {
    AhRational time_base;
    AhRational pkt_timebase;
    AhRational framerate;
} AhCtxFps61;

uint8_t *ah_av_buffer_data(void *buf_ref) {
    if (!buf_ref) return NULL;
    return ((AhBufferRef *)buf_ref)->data;
}

int ah_av_hwframes_configure(
    void *frames_ref,
    int avutil_major,
    int pool_size,
    int hw_fmt,
    int sw_fmt,
    int w,
    int h
) {
    uint8_t *data = ah_av_buffer_data(frames_ref);
    if (!data) return -1;
    if (avutil_major == 58) {
        AhHWFramesContext58 *fc = (AhHWFramesContext58 *)data;
        fc->initial_pool_size = pool_size;
        fc->format = hw_fmt;
        fc->sw_format = sw_fmt;
        fc->width = w;
        fc->height = h;
        return 0;
    }
    if (avutil_major == 59 || avutil_major == 60) {
        AhHWFramesContext59 *fc = (AhHWFramesContext59 *)data;
        fc->initial_pool_size = pool_size;
        fc->format = hw_fmt;
        fc->sw_format = sw_fmt;
        fc->width = w;
        fc->height = h;
        return 0;
    }
    return -1;
}

int ah_av_frame_set_params(void *frame, int avutil_major, int w, int h, int format) {
    if (!frame) return -1;
    if (avutil_major == 58 || avutil_major == 59) {
        AhFrame58 *f = (AhFrame58 *)frame;
        f->width = w;
        f->height = h;
        f->format = format;
        return 0;
    }
    if (avutil_major == 60) {
        AhFrame60 *f = (AhFrame60 *)frame;
        f->width = w;
        f->height = h;
        f->format = format;
        return 0;
    }
    return -1;
}

uint8_t *ah_av_frame_plane(void *frame, int avutil_major, int plane, int *linesize_out) {
    if (!frame || plane < 0 || plane >= 8) return NULL;
    if (avutil_major == 58 || avutil_major == 59) {
        AhFrame58 *f = (AhFrame58 *)frame;
        if (linesize_out) *linesize_out = f->linesize[plane];
        return f->data[plane];
    }
    if (avutil_major == 60) {
        AhFrame60 *f = (AhFrame60 *)frame;
        if (linesize_out) *linesize_out = f->linesize[plane];
        return f->data[plane];
    }
    return NULL;
}

int ah_av_frame_set_pict_type(void *frame, int avutil_major, int pict_type) {
    if (!frame) return -1;
    if (avutil_major == 58 || avutil_major == 59) {
        ((AhFrame58 *)frame)->pict_type = pict_type;
        return 0;
    }
    if (avutil_major == 60) {
        ((AhFrame60 *)frame)->pict_type = pict_type;
        return 0;
    }
    return -1;
}

int ah_av_frame_set_pts(void *frame, int avutil_major, int64_t pts) {
    if (!frame) return -1;
    if (avutil_major == 58 || avutil_major == 59) {
        ((AhFrame58 *)frame)->pts = pts;
        return 0;
    }
    if (avutil_major == 60) {
        ((AhFrame60 *)frame)->pts = pts;
        return 0;
    }
    return -1;
}

const uint8_t *ah_av_packet_data(const void *pkt) {
    if (!pkt) return NULL;
    return ((const AhPacket *)pkt)->data;
}

int ah_av_packet_size(const void *pkt) {
    if (!pkt) return 0;
    return ((const AhPacket *)pkt)->size;
}

int ah_av_packet_flags(const void *pkt) {
    if (!pkt) return 0;
    return ((const AhPacket *)pkt)->flags;
}

int ah_av_ctx_hw_frames_offset(
    int avcodec_major,
    int off_max_pixels,
    int off_hwaccel_flags,
    int off_extra_hw_frames
) {
    if (avcodec_major == 60) {
        if (off_max_pixels < (int)offsetof(AhCtxHw60, max_pixels) || off_hwaccel_flags < 0) {
            return -1;
        }
        int expected = (int)(offsetof(AhCtxHw60, hwaccel_flags) - offsetof(AhCtxHw60, max_pixels));
        if (off_hwaccel_flags - off_max_pixels != expected) {
            return -1;
        }
        int off = off_max_pixels - (int)offsetof(AhCtxHw60, max_pixels);
        return (off % (int)sizeof(void *) == 0) ? off : -1;
    }
    if (avcodec_major == 61 || avcodec_major == 62) {
        if (off_hwaccel_flags < (int)offsetof(AhCtxHw61, hwaccel_flags) || off_extra_hw_frames < 0) {
            return -1;
        }
        int expected =
            (int)(offsetof(AhCtxHw61, extra_hw_frames) - offsetof(AhCtxHw61, hwaccel_flags));
        if (off_extra_hw_frames - off_hwaccel_flags != expected) {
            return -1;
        }
        int off = off_hwaccel_flags - (int)offsetof(AhCtxHw61, hwaccel_flags);
        return (off % (int)sizeof(void *) == 0) ? off : -1;
    }
    return -1;
}

int ah_av_ctx_framerate_offset(int avcodec_major, int off_time_base, int off_pkt_timebase) {
    if (avcodec_major == 60) {
        if (off_pkt_timebase < (int)offsetof(AhCtxFps60, pkt_timebase)) {
            return -1;
        }
        int off = off_pkt_timebase - (int)offsetof(AhCtxFps60, pkt_timebase);
        return (off % (int)sizeof(int) == 0) ? off : -1;
    }
    if (avcodec_major == 61 || avcodec_major == 62) {
        if (off_time_base < 0 || off_pkt_timebase < 0) {
            return -1;
        }
        int expected =
            (int)(offsetof(AhCtxFps61, pkt_timebase) - offsetof(AhCtxFps61, time_base));
        if (off_pkt_timebase - off_time_base != expected) {
            return -1;
        }
        return off_time_base + (int)offsetof(AhCtxFps61, framerate);
    }
    return -1;
}

void ah_av_ctx_set_hw_frames_ctx(void *ctx, int offset, void *frames_ref) {
    if (!ctx || offset < 0) return;
    void **slot = (void **)((uint8_t *)ctx + offset);
    *slot = frames_ref;
}

void ah_av_ctx_set_framerate(void *ctx, int offset, int num, int den) {
    if (!ctx || offset < 0) return;
    AhRational *r = (AhRational *)((uint8_t *)ctx + offset);
    r->num = num;
    r->den = den;
}
