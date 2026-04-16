# Cross-compile toolchain image. Source is bind-mounted at /app (see build.sh).
#
# docker build -t rusty-cam-builder .
# docker run --rm -v "$(pwd):/app" -v "$(pwd)/target:/app/target" -w /app rusty-cam-builder

FROM ubuntu:22.04

# Set timezone to avoid interactive prompts
ENV TZ=UTC
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# Install dependencies including cross-compilation tools
RUN DEBIAN_FRONTEND=noninteractive apt-get update && apt-get install -y \
    curl \
    build-essential \
    gcc-aarch64-linux-gnu \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Add arm64 architecture and install GStreamer libraries for target
RUN dpkg --add-architecture arm64 && \
    apt-get update && \
    DEBIAN_FRONTEND=noninteractive apt-get install -y \
    libgstreamer1.0-dev:arm64 \
    libgstreamer-plugins-base1.0-dev:arm64 \
    libgstreamer-plugins-good1.0-dev:arm64 \
    libgstreamer-plugins-bad1.0-dev:arm64 \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Add aarch64 target
RUN rustup target add aarch64-unknown-linux-gnu

# Configure cargo for cross-compilation
RUN mkdir -p /root/.cargo && \
    echo '[target.aarch64-unknown-linux-gnu]' >> /root/.cargo/config.toml && \
    echo 'linker = "aarch64-linux-gnu-gcc"' >> /root/.cargo/config.toml

# Set environment variables for cross-compilation
ENV PKG_CONFIG_ALLOW_CROSS=1
ENV PKG_CONFIG_PATH_aarch64_unknown_linux_gnu=/usr/lib/aarch64-linux-gnu/pkgconfig

WORKDIR /app

# Manifests + sources Cargo needs to see a valid package (otherwise: "no targets
# specified"). This layer still caches until Cargo.toml/Cargo.lock or src/ change.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# Older Cargo only supports `cargo fetch --target` (no --features). This still
# populates the registry cache; `cargo build` resolves linux-only deps later.
RUN cargo fetch --target aarch64-unknown-linux-gnu

CMD ["cargo", "build", "--target", "aarch64-unknown-linux-gnu", "--release", "--no-default-features", "--features", "linux"]
