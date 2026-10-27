#!/usr/bin/env ruby
# Run the deployment's actual shell block with real PENDING detection and fake
# package/media commands. No network, package installs or deployments.
require "fileutils"
require "open3"
require "pathname"
require "tmpdir"
require "yaml"

root = Pathname.new(__dir__).join("..").realpath
# The strict dashboard proof reads frozen Git objects before installer staging.
# Every current-source marketing caller must obtain history in its own job.
def require_source_history(workflow, job)
  steps = workflow.fetch("jobs").fetch(job).fetch("steps")
  checkout = steps.find { |entry| entry.fetch("uses", "").start_with?("actions/checkout@") }
  raise "#{job}: marketing provenance requires fetch-depth: 0" unless checkout && checkout.fetch("with", {})["fetch-depth"] == 0
end
[
  ["ci.yml", "marketing-site"],
  ["marketing-deploy.yml", "deploy"],
  ["release.yml", "marketing-site"],
].each do |filename, job|
  workflow = YAML.load_file(root.join(".github/workflows", filename))
  require_source_history(workflow, job)
  checkout = workflow.fetch("jobs").fetch(job).fetch("steps").find { |entry| entry.fetch("uses", "").start_with?("actions/checkout@") }
  checkout.fetch("with")["fetch-depth"] = 1
  rejected = false
  begin
    require_source_history(workflow, job)
  rescue RuntimeError => error
    rejected = error.message.include?("requires fetch-depth: 0")
  end
  abort "FAIL: #{filename}: shallow marketing checkout was accepted" unless rejected
  puts "ok: #{filename} marketing source history is complete; shallow mutation refused"
end

bodies = [
  ["marketing-deploy.yml", "deploy", "Install media inspection dependencies"],
  ["release.yml", "marketing-site", "Install film inspection dependency"],
].map do |filename, job, name|
  workflow = YAML.load_file(root.join(".github/workflows", filename))
  step = workflow.fetch("jobs").fetch(job).fetch("steps").find { |entry| entry["name"] == name }
  abort "#{filename}: marketing dependency step is missing or conditionally bypassed" unless step && step["run"] && !step.key?("if")
  [filename, step.fetch("run")]
end
cases = [
  {name: "matching PENDING needs no tools or apt", marker: :current, apt: [], success: true, probes: false},
  {name: "matching PENDING with dashboard video reuses media tools", marker: :current, dashboard: true, ffmpeg: true, ffprobe: true, apt: [], success: true},
  {name: "matching PENDING with dashboard video installs missing tools", marker: :current, dashboard: true, apt: %w[update install], success: true},
  {name: "dashboard video cannot hide a failed dependency install", marker: :current, dashboard: true, fail: "install", apt: %w[update install], success: false},
  {name: "stale PENDING reuses working tools", marker: :stale, ffmpeg: true, ffprobe: true, apt: [], success: true},
  {name: "missing PENDING installs missing tools", marker: :missing, apt: %w[update install], success: true},
  {name: "symlink PENDING cannot skip dependencies", marker: :symlink, apt: %w[update install], success: true},
  {name: "empty PENDING cannot skip dependencies", marker: :empty, apt: %w[update install], success: true},
  {name: "missing ffprobe needs installation", marker: :missing, ffmpeg: true, apt: %w[update install], success: true},
  {name: "missing ffmpeg needs installation", marker: :missing, ffprobe: true, apt: %w[update install], success: true},
  {name: "apt update failure stops installation", marker: :missing, fail: "update", apt: %w[update], success: false},
  {name: "apt install failure stops deployment", marker: :missing, fail: "install", apt: %w[update install], success: false},
  {name: "successful apt cannot hide broken tools", marker: :missing, broken_install: true, apt: %w[update install], success: false},
]
bodies.each do |filename, body|
cases.each do |test|
  Dir.mktmpdir("axocoatl-marketing-dependency") do |directory|
    dir = Pathname.new(directory)
    %w[bin scripts axocoatl-cli demo/one-app/films].each { |path| dir.join(path).mkpath }
    FileUtils.cp(root.join("scripts/verify-film-gate.sh"), dir.join("scripts/verify-film-gate.sh"))
    dir.join("axocoatl-cli/Cargo.toml").write("[package]\nversion = \"1.5.0\"\n")
    dir.join("demo/one-app/films/verify-film-set.mjs").write("// Not invoked by pending mode.\n")
    if test[:dashboard]
      video = dir.join("sites/marketing/assets/demo/dashboard-1.5/dashboard-tour.mp4")
      video.dirname.mkpath
      video.write("dependency-presence fixture; no media probing in this shell contract")
    end
    marker = dir.join("demo/one-app/films/PENDING")
    case test.fetch(:marker)
    when :current then marker.write("1.5.0\n")
    when :stale then marker.write("1.4.0\n")
    when :empty then marker.write("\n")
    when :symlink
      dir.join("marker-target").write("1.5.0\n")
      File.symlink(dir.join("marker-target"), marker)
    end
    %w[ffmpeg ffprobe].each do |tool|
      command = dir.join("bin", tool)
      command.write(<<~SH)
        #!/bin/bash
        set -eu
        printf '%s\\n' '#{tool}' >> "$AXO_TEST_LOG"
        [[ "$*" == -version ]] || exit 92
        if [[ "$AXO_TEST_#{tool.upcase}" == 1 ]] || \\
           [[ -f "$AXO_TEST_STATE/installed" && "$AXO_TEST_INSTALL_WORKS" == 1 ]]; then
          echo '#{tool} fixture version'
          exit 0
        fi
        exit 127
      SH
      command.chmod(0755)
    end
    sudo = dir.join("bin/sudo")
    sudo.write(<<~'SH')
      #!/bin/bash
      set -eu
      if [[ "$*" == 'apt-get update' ]]; then
        operation=update
      elif [[ "$*" == 'apt-get install -y ffmpeg' ]]; then
        operation=install
      else
        echo 'unexpected package command' >&2
        exit 93
      fi
      printf 'apt:%s\n' "$operation" >> "$AXO_TEST_LOG"
      [[ "$AXO_TEST_APT_FAIL" != "$operation" ]] || exit 71
      if [[ "$operation" == install ]]; then touch "$AXO_TEST_STATE/installed"; fi
    SH
    sudo.chmod(0755)
    env = {
      "PATH" => "#{dir.join('bin')}:/usr/bin:/bin",
      "AXO_TEST_LOG" => dir.join("calls").to_s,
      "AXO_TEST_STATE" => directory,
      "AXO_TEST_FFMPEG" => test[:ffmpeg] ? "1" : "0",
      "AXO_TEST_FFPROBE" => test[:ffprobe] ? "1" : "0",
      "AXO_TEST_INSTALL_WORKS" => test[:broken_install] ? "0" : "1",
      "AXO_TEST_APT_FAIL" => test.fetch(:fail, ""),
      "AXO_FILM_REPO_ROOT" => nil,
      "AXO_FILM_VERIFIER" => nil,
    }
    stdout, stderr, status = Open3.capture3(env, "/bin/bash", "-euo", "pipefail", "-c", body, chdir: directory)
    calls = dir.join("calls").exist? ? dir.join("calls").read.lines.map(&:strip) : []
    actual_apt = calls.grep(/^apt:/).map { |line| line.delete_prefix("apt:") }
    unless status.success? == test.fetch(:success) && actual_apt == test.fetch(:apt)
      abort "FAIL: #{test[:name]}: exit=#{status.exitstatus}, calls=#{calls.inspect}\n#{stdout}#{stderr}"
    end
    if test[:probes] == false && !calls.empty?
      abort "FAIL: #{test[:name]} probed or installed unnecessary tools: #{calls.inspect}"
    end
    if test[:success] && test[:probes] != false && !%w[ffmpeg ffprobe].all? { |tool| calls.include?(tool) }
      abort "FAIL: #{test[:name]} did not verify both media tools"
    end
    puts "ok: #{filename}: #{test[:name]}"
  end
end
end
puts "Marketing deployment dependency contract: PASS (#{cases.length * bodies.length} dependency cases; 3 history bindings and shallow-checkout refusals)"
