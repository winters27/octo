FROM rust:1-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends \
      clang pkg-config libavformat-dev libavcodec-dev libavutil-dev libswresample-dev \
      libavfilter-dev libavdevice-dev libswscale-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# The lock file pins the exact crates CI tested.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:trixie-slim
# FFmpeg's libraries for decoding (FLAC, MP3, AAC, Opus, ...); nothing else.
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/octo-sonic /usr/local/bin/octo-sonic
ENV MUSIC_ROOT=/music
EXPOSE 8080
# It only reads: nobody, unless compose says otherwise (SONIC_USER, for a music folder only
# its owner can read).
USER 65534:65534
CMD ["octo-sonic"]
