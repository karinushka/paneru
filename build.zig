const std = @import("std");

pub fn build(b: *std.Build) void {
    const target = b.standardTargetOptions(.{});
    const optimize = b.standardOptimizeOption(.{});

    const lib_mod = b.createModule(.{
        .root_source_file = b.path("src/root.zig"),
        .target = target,
        .optimize = optimize,
    });

    const private_framework = "/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk/System/Library/PrivateFrameworks";
    const carbon_framework = "/Library/Developer/CommandLineTools/SDKs/MacOSX15.sdk/System/Library/Frameworks/Carbon.framework/Frameworks";

    const dynamic_library = b.addSharedLibrary(.{
        .name = "private",
        .root_module = lib_mod,
    });
    dynamic_library.linkFramework("CoreFoundation");
    dynamic_library.linkFramework("ApplicationServices");
    dynamic_library.addFrameworkPath(.{ .cwd_relative = private_framework });
    dynamic_library.linkFramework("SkyLight");
    dynamic_library.addFrameworkPath(.{ .cwd_relative = carbon_framework });
    dynamic_library.linkFramework("HIToolbox");

    const static_library = b.addStaticLibrary(.{
        .name = "private",
        .root_module = lib_mod,
    });
    static_library.linkFramework("CoreFoundation");
    static_library.linkFramework("ApplicationServices");
    static_library.addFrameworkPath(.{ .cwd_relative = private_framework });
    static_library.linkFramework("SkyLight");
    static_library.addFrameworkPath(.{ .cwd_relative = carbon_framework });
    static_library.linkFramework("HIToolbox");

    b.installArtifact(dynamic_library);
    b.installArtifact(static_library);
}
