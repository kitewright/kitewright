# Homebrew formula for kitewright (installs the `kite` binary).
#
# TAP TBD: this is a template for a future `kitewright/homebrew-tap` repo
# (`brew install kitewright/tap/kitewright`). Until that tap is published this
# formula is not resolvable by `brew`. Per release, bump `version` and replace
# each `sha256` with the checksum of the matching `kite-<target>.tar.gz` asset
# produced by .github/workflows/release.yml, e.g.:
#   shasum -a 256 kite-aarch64-apple-darwin.tar.gz
class Kitewright < Formula
  desc "Lightweight browser-automation MCP server for AI agents (single binary)"
  homepage "https://github.com/kitewright/kitewright"
  version "0.1.0"
  license "MIT"

  BASE = "https://github.com/kitewright/kitewright/releases/download/v#{version}".freeze

  on_macos do
    on_arm do
      url "#{BASE}/kite-aarch64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill per release
    end
    on_intel do
      url "#{BASE}/kite-x86_64-apple-darwin.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill per release
    end
  end

  on_linux do
    on_arm do
      url "#{BASE}/kite-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill per release
    end
    on_intel do
      url "#{BASE}/kite-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "0000000000000000000000000000000000000000000000000000000000000000" # TODO: fill per release
    end
  end

  def install
    bin.install "kite"
  end

  test do
    # `kite install --help` runs without a browser and prints its usage.
    assert_match "kite install", shell_output("#{bin}/kite install --help")
  end
end
