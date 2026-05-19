#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 3 ]]; then
  echo "usage: $0 <owner/repo> <tag> <dist-dir>" >&2
  exit 2
fi

repo="$1"
tag="$2"
dist_dir="$3"
version="${tag#v}"

sha_for() {
  local archive="$1"
  awk '{print $1}' "${dist_dir}/${archive}.sha256"
}

mac_arm="cctty-${version}-aarch64-apple-darwin.tar.gz"
mac_intel="cctty-${version}-x86_64-apple-darwin.tar.gz"
linux_x64="cctty-${version}-x86_64-unknown-linux-gnu.tar.gz"

cat <<FORMULA
class Cctty < Formula
  desc "Drop-in Claude Agent SDK runner backed by the interactive Claude TTY"
  homepage "https://github.com/${repo}"
  version "${version}"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "https://github.com/${repo}/releases/download/${tag}/${mac_arm}"
      sha256 "$(sha_for "${mac_arm}")"
    else
      url "https://github.com/${repo}/releases/download/${tag}/${mac_intel}"
      sha256 "$(sha_for "${mac_intel}")"
    end
  end

  on_linux do
    if Hardware::CPU.intel?
      url "https://github.com/${repo}/releases/download/${tag}/${linux_x64}"
      sha256 "$(sha_for "${linux_x64}")"
    end
  end

  def install
    bin.install "cctty"
  end

  test do
    assert_path_exists bin/"cctty"
    assert_predicate bin/"cctty", :executable?
  end
end
FORMULA
