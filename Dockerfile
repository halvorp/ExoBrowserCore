FROM rust:bookworm AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libwpewebkit-1.1-dev \
    libwpebackend-fdo-1.0-dev \
    libwayland-dev \
    libglib2.0-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY gutted-proto ./gutted-proto
COPY gutted-host-rs ./gutted-host-rs
COPY gutted-client ./gutted-client
COPY gutted-client-gtk ./gutted-client-gtk
COPY host ./host

RUN cargo build --release -p gutted-host

FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y --no-install-recommends \
    libwpewebkit-1.1-0 \
    libwpebackend-fdo-1.0-1 \
    libwayland-server0 \
    libwayland-client0 \
    libwayland-egl1 \
    libegl1 \
    libegl-mesa0 \
    libgl1-mesa-dri \
    libglx-mesa0 \
    libgbm1 \
    libdrm2 \
    glib-networking \
    gstreamer1.0-plugins-base \
    gstreamer1.0-plugins-good \
    gstreamer1.0-plugins-bad \
    gstreamer1.0-plugins-ugly \
    gstreamer1.0-gl \
    gstreamer1.0-libav \
    gstreamer1.0-pulseaudio \
    pulseaudio \
    pulseaudio-utils \
    ca-certificates \
    fonts-dejavu-core \
    fonts-liberation \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/gutted-host /usr/local/bin/gutted-host

ENV GBROWSER_LISTEN=0.0.0.0:4433
ENV GBROWSER_WEB_LISTEN=0.0.0.0:8080
ENV LIBGL_ALWAYS_SOFTWARE=1
ENV WEBKIT_GST_DMABUF_SINK_ENABLED=0
ENV WEBKIT_GST_MAX_AVC1_RESOLUTION=720P
ENV GST_PLUGIN_FEATURE_RANK=vah264dec:NONE,vaav1dec:NONE,vavp9dec:NONE,nvdec:NONE,openh264dec:NONE,avdec_h264:MAX
ENV XDG_RUNTIME_DIR=/tmp/xdg-runtime

RUN mkdir -p /tmp/xdg-runtime && chmod 0700 /tmp/xdg-runtime

EXPOSE 4433/udp 8080/tcp

ENTRYPOINT ["/usr/local/bin/gutted-host"]
