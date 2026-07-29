FROM ubuntu:22.04

# Avoid interactive prompts during package installations
ENV DEBIAN_FRONTEND=noninteractive

# Install essential Yocto Project compilation and QEMU rendering dependencies
RUN apt-get update && apt-get install -y --no-install-recommends \
    bc \
    build-essential \
    bsdmainutils \
    chrpath \
    cpio \
    debianutils \
    diffstat \
    file \
    gawk \
    gedit \
    gdisk \
    git \
    gcc \
    iputils-ping \
    iptables \
    libegl1-mesa \
    libsdl2-dev \
    libssl-dev \
    locales \
    locales-all \
    lz4 \
    nano \
    pylint \
    python3 \
    python3-pip \
    python3-pexpect \
    python3-git \
    python3-jinja2 \
    python-is-python3 \
    qemu-system-arm \
    qemu-system-x86 \
    socat \
    sudo \
    texinfo \
    u-boot-tools \
    unzip \
    wget \
    xterm \
    xz-utils \
    zstd \
    && rm -rf /var/lib/apt/lists/*

# gcc-multilib/g++-multilib are x86 32-bit multiarch (i386) compatibility
# packages - they only exist in Ubuntu's amd64 archives. Installing them
# unconditionally breaks the image build outright on arm64 hosts (Apple
# Silicon Macs via Docker Desktop, arm64 CI runners, etc). Yocto's own
# host-package docs list these as amd64-only requirements, so skip them
# entirely on non-amd64 build hosts rather than failing the build.
RUN if [ "$(dpkg --print-architecture)" = "amd64" ]; then \
        apt-get update && apt-get install -y --no-install-recommends \
            gcc-multilib \
            g++-multilib \
        && rm -rf /var/lib/apt/lists/*; \
    else \
        echo "Skipping gcc-multilib/g++-multilib: not available on $(dpkg --print-architecture)"; \
    fi

# Set up system locales to prevent BitBake parsing warnings
RUN locale-gen en_US.UTF-8
ENV LANG=en_US.UTF-8
ENV LANGUAGE=en_US:en
ENV LC_ALL=en_US.UTF-8

# Create a non-root development user matching standard Yocto constraints
ARG USER_ID=1000
ARG GROUP_ID=1000
RUN groupadd -g ${GROUP_ID} builder && \
    useradd -u ${USER_ID} -g builder -m -s /bin/bash builder && \
    echo "builder ALL=(ALL) NOPASSWD:ALL" >> /etc/sudoers

USER builder
WORKDIR /home/builder/workspace