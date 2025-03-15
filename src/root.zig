const std = @import("std");

const frameworks = "/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk/System/Library/Frameworks/";
const ca = @cImport({
    @cInclude(frameworks ++ "Carbon.framework/Versions/A/Frameworks/HIToolbox.framework/Versions/A/Headers/CarbonEvents.h");
    @cInclude(frameworks ++ "Kernel.framework/Versions/A/Headers/sys/types.h");
});
extern "c" fn SLPSGetFrontProcess(*ca.ProcessSerialNumber) void;

const Errors = error{
    Internal,
    OutOfMemory,
};

fn process_handler(_: ca.EventHandlerCallRef, event: ca.EventRef, context: ?*anyopaque) callconv(.c) ca.OSStatus {
    const callback: *ProcessCallback = @ptrCast(@alignCast(context));
    if (callback.callback != null and callback.context != null) {
        var psn: ca.ProcessSerialNumber = undefined;
        if (ca.noErr != ca.GetEventParameter(event, ca.kEventParamProcessID, ca.typeProcessSerialNumber, null, @sizeOf(ca.ProcessSerialNumber), null, &psn)) {
            return -1;
        }

        const decoded_event = ca.GetEventKind(event);
        return callback.callback.?(callback.context.?, &psn, decoded_event);
    }
    return 0;
}

fn cfstring_copy(allocator: std.mem.Allocator, string: ca.CFStringRef) ![]u8 {
    const num_bytes = ca.CFStringGetMaximumSizeForEncoding(ca.CFStringGetLength(string), ca.kCFStringEncodingUTF8);
    const output: [*c]u8 = @ptrCast(try allocator.alloc(u8, @bitCast(num_bytes + 1)));
    if (1 > ca.CFStringGetCString(string, output, num_bytes + 1, ca.kCFStringEncodingUTF8)) {
        return Errors.Internal;
    }
    return output[0..@bitCast(num_bytes + 1)];
}

fn add_running_processes(callback: ?*process_callback, self: ?*anyopaque) !void {
    var psn = ca.ProcessSerialNumber{ .highLongOfPSN = ca.kNoProcess, .lowLongOfPSN = ca.kNoProcess };
    while (ca.GetNextProcess(&psn) == ca.noErr) {
        // Fake event to add an existing App.
        _ = callback.?(self.?, &psn, ca.kEventAppLaunched);
    }
}

pub export fn get_process_info(psn: *ca.ProcessSerialNumber, process: *Process) callconv(.c) ca.OSStatus {
    var process_info = ca.ProcessInfoRec{ .processInfoLength = @sizeOf(ca.ProcessInfoRec) };
    const err = ca.GetProcessInformation(psn, &process_info);
    if (err != 0) {
        std.debug.print("Error fetching process information: {x}\n", .{err});
        return 1;
    }

    // struct ProcessInfoRec {
    //   UInt32              processInfoLength;
    //   StringPtr           processName;
    //   ProcessSerialNumber  processNumber;
    //   UInt32              processType;
    //   OSType              processSignature;
    //   UInt32              processMode;
    //   Ptr                 processLocation;
    //   UInt32              processSize;
    //   UInt32              processFreeMem;
    //   ProcessSerialNumber  processLauncher;
    //   UInt32              processLaunchDate;
    //   UInt32              processActiveTime;
    //   FSRefPtr            processAppRef;
    // };

    _ = ca.CopyProcessName(psn, &process.name);

    _ = ca.GetProcessPID(psn, &process.pid);
    process.terminated = false;

    return 0;
}

const Process = extern struct {
    pid: c_int,
    name: ca.CFStringRef,
    ns_application: ?*anyopaque,
    policy: i32,
    terminated: bool,
};

const process_callback = fn (self: ?*anyopaque, psn: *ca.ProcessSerialNumber, event: c_uint) callconv(.c) ca.OSStatus;

const ProcessCallback = struct {
    callback: ?*process_callback,
    context: ?*anyopaque,
};

var callback_data: ProcessCallback = .{ .callback = null, .context = null };

pub export fn setup_process_handler(callback: ?*process_callback, self: ?*anyopaque) callconv(.c) ?*anyopaque {
    const pool = AutoreleasePool.init(); // [[NSAutoreleasePool alloc] init];
    defer pool.deinit(); // [pool drain];

    const target = ca.GetApplicationEventTarget();
    const event_types: [3]ca.EventTypeSpec = .{
        ca.EventTypeSpec{ .eventClass = ca.kEventClassApplication, .eventKind = ca.kEventAppLaunched },
        ca.EventTypeSpec{ .eventClass = ca.kEventClassApplication, .eventKind = ca.kEventAppTerminated },
        ca.EventTypeSpec{ .eventClass = ca.kEventClassApplication, .eventKind = ca.kEventAppFrontSwitched },
    };
    std.debug.print("Target: {?}\n", .{target});

    if (callback != null and self != null) {
        add_running_processes(callback.?, self.?) catch return null;
    }

    var front_psn: ca.ProcessSerialNumber = .{ .highLongOfPSN = 0, .lowLongOfPSN = 0 };
    SLPSGetFrontProcess(&front_psn);

    var front_pid: ca.pid_t = undefined;
    _ = ca.GetProcessPID(&front_psn, &front_pid);

    const switch_event_time: ca.EventTime = ca.GetCurrentEventTime();
    std.debug.print("\nFront Process {x}:{x} - pid {d} - last event {x}\n", .{ front_psn.highLongOfPSN, front_psn.lowLongOfPSN, front_pid, switch_event_time });

    callback_data.callback = callback.?;
    callback_data.context = self.?;
    var handler_ref: ca.EventHandlerRef = undefined;
    if (0 != ca.InstallEventHandler(target, process_handler, event_types.len, &event_types, &callback_data, &handler_ref)) {
        std.debug.print("Error installing event handler\n", .{});
        return null;
    }
    return handler_ref;
}

pub export fn remove_process_handler(handle: ca.EventHandlerRef) callconv(.c) void {
    _ = ca.RemoveEventHandler(handle);
}

const AutoreleasePool = opaque {
    /// Create a new autorelease pool. To clean it up, call deinit.
    inline fn init() *AutoreleasePool {
        return @ptrCast(objc_autoreleasePoolPush().?);
    }

    inline fn deinit(self: *AutoreleasePool) void {
        objc_autoreleasePoolPop(self);
    }
};

// I'm not sure if these are internal or not... they aren't in any headers,
// but its how autorelease pools are implemented.
extern "c" fn objc_autoreleasePoolPush() ?*anyopaque;
extern "c" fn objc_autoreleasePoolPop(?*anyopaque) void;
