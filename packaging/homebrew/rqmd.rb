# Homebrew formula for rqmd.
#
# This file lives in the rqmd source repo at packaging/homebrew/rqmd.rb.
# The Homebrew tap (github.com/tylern91/homebrew-rqmd) receives an updated
# copy automatically on each release via scripts/update-homebrew-formula.sh.
#
# Install:
#   brew tap tylern91/rqmd
#   brew install rqmd
class Rqmd < Formula
  desc "Hybrid local document search in a single static binary"
  homepage "https://github.com/tylern91/rqmd"
  license "MIT"

  on_macos do
    on_arm do
      # aarch64-apple-darwin (macOS, Apple Silicon)
      url "https://github.com/tylern91/rqmd/releases/download/RQMD_VERSION/rqmd-RQMD_VERSION-aarch64-apple-darwin.tar.gz"
      sha256 "RQMD_SHA256_MACOS_ARM64"
      version "RQMD_BARE_VERSION"
    end
  end

  on_linux do
    on_intel do
      # x86_64-unknown-linux-gnu (Linux, Intel/AMD 64-bit)
      url "https://github.com/tylern91/rqmd/releases/download/RQMD_VERSION/rqmd-RQMD_VERSION-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "RQMD_SHA256_LINUX_X86"
      version "RQMD_BARE_VERSION"
    end
  end

  def install
    bin.install "rqmd"
  end

  test do
    system "#{bin}/rqmd", "--version"
  end
end
