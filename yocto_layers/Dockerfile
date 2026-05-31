FROM ubuntu:22.04

# Avoid interactive prompts during package installations
ENV DEBIAN_FRONTEND=noninteractive

# Install essential Yocto Project compilation and QEMU rendering dependencies
RUN apt-get update && apt-get install -y \
    bc \
    build-essential \
    bsdmainutils \
    chrpath \
    cpio \
    debianutils \
    diffstat \
    gawk \
    gedit \
    gdisk \
    git \
    gcc \
    iputils-ping \
    iptables \
    libegl1-mesa \
    libsdl1.2-dev \
    libssl-dev \
    locales \
    locales-all \
    multilib-build-essential \
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
    && rm -rf /var/lib/apt/lists/*

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