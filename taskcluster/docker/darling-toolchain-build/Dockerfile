FROM $DOCKER_IMAGE_PARENT
MAINTAINER Jamie Nicol <jnicol@mozilla.com>

VOLUME /builds/worker/checkouts
VOLUME /builds/worker/workspace
VOLUME /builds/worker/tooltool-cache

# Packages needed to build darling (https://github.com/darlinghq/darling),
# on top of what deb12-toolchain-build already provides. See
# https://docs.darlinghq.org/build-instructions.html
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
      libfuse-dev \
      libudev-dev \
      libc6-dev-i386 \
      libcairo2-dev \
      libgl1-mesa-dev \
      libglu1-mesa-dev \
      libtiff5-dev \
      libfreetype6-dev \
      git-lfs \
      libelf-dev \
      libegl1-mesa-dev \
      libfontconfig1-dev \
      libbsd-dev \
      libxrandr-dev \
      libxcursor-dev \
      libgif-dev \
      libavutil-dev \
      libpulse-dev \
      libavformat-dev \
      libavcodec-dev \
      libswresample-dev \
      libdbus-1-dev \
      libxkbfile-dev \
      libcap2-bin

# Set a default command useful for debugging
CMD ["/bin/bash", "--login"]
