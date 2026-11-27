#!/bin/sh
# Prints the Homebrew formula for a release:  packaging/homebrew-formula.sh VERSION SHA256SUMS
# (the tap is prabhask5/homebrew-tap; the release workflow commits the output there as Formula/moi.rb)
set -eu
version=$1
sums=$2
sum() { awk -v f="moi-$version-$1.tar.gz" '$2 == f { print $1 }' "$sums"; }
x86=$(sum x86_64-unknown-linux-musl)
arm=$(sum aarch64-unknown-linux-musl)
[ -n "$x86" ] && [ -n "$arm" ] || { echo "SHA256SUMS lacks a tarball of $version" >&2; exit 1; }
base="https://github.com/prabhask5/moi/releases/download/v$version"
cat <<FORMULA
class Moi < Formula
  desc "One binary that runs a Linux server: state, checks, backups, rebuilds"
  homepage "https://github.com/prabhask5/moi"
  version "$version"
  license "MIT"

  depends_on :linux

  on_intel do
    url "$base/moi-$version-x86_64-unknown-linux-musl.tar.gz"
    sha256 "$x86"
  end
  on_arm do
    url "$base/moi-$version-aarch64-unknown-linux-musl.tar.gz"
    sha256 "$arm"
  end

  def install
    bin.install "moi"
    man1.install "moi.1"
    bash_completion.install "completions/moi.bash" => "moi"
    zsh_completion.install "completions/_moi"
    fish_completion.install "completions/moi.fish"
  end

  def caveats
    <<~EOS
      moi looks after the whole machine and runs as root, which Homebrew does not.
      Put the binary where the system service can start it, then set the server up:
        sudo install -m 755 #{opt_bin}/moi /usr/local/bin/moi
        sudo moi init
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/moi --version")
  end
end
FORMULA
