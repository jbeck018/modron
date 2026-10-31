# Homebrew formula — source build. Bottles follow the first tagged release.
class Modron < Formula
  desc "Token-efficient Rust AI coding agent harness"
  homepage "https://github.com/jbeck018/modron"
  url "https://github.com/jbeck018/modron.git",
      tag:      "v0.1.0",
      revision: "1ec0481d91c110a5fadb3a5297d30a27f45ab2f6"
  version "0.1.0"
  license "MIT"

  depends_on "rust" => :build

  def install
    system "cargo", "install", "--locked", "--path", "crates/modron-cli"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/modron --version")
    system "#{bin}/modron", "smoke", "--provider", "mock"
  end
end
