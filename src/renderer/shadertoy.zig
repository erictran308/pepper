const std = @import("std");
const builtin = @import("builtin");
const Allocator = std.mem.Allocator;
const ArenaAllocator = std.heap.ArenaAllocator;
const glslang = @import("glslang");
const spvcross = @import("spirv_cross");
const configpkg = @import("../config.zig");
const compat_file = @import("../lib/compat/file.zig");
const global = @import("../global.zig");

const log = std.log.scoped(.shadertoy);

/// The uniform struct used for shadertoy shaders.
pub const Uniforms = extern struct {
    resolution: [3]f32 align(16),
    time: f32 align(4),
    time_delta: f32 align(4),
    frame_rate: f32 align(4),
    frame: i32 align(4),
    channel_time: [4][4]f32 align(16),
    channel_resolution: [4][4]f32 align(16),
    mouse: [4]f32 align(16),
    date: [4]f32 align(16),
    sample_rate: f32 align(4),
    current_cursor: [4]f32 align(16),
    previous_cursor: [4]f32 align(16),
    current_cursor_color: [4]f32 align(16),
    previous_cursor_color: [4]f32 align(16),
    current_cursor_style: i32 align(4),
    previous_cursor_style: i32 align(4),
    cursor_visible: i32 align(4),
    cursor_change_time: f32 align(4),
    time_focus: f32 align(4),
    focus: i32 align(4),
    palette: [256][4]f32 align(16),
    background_color: [4]f32 align(16),
    foreground_color: [4]f32 align(16),
    cursor_color: [4]f32 align(16),
    cursor_text: [4]f32 align(16),
    selection_foreground_color: [4]f32 align(16),
    selection_background_color: [4]f32 align(16),
};

/// The target to load shaders for.
pub const Target = enum { glsl, msl };

/// Load a set of shaders from files and convert them to the target
/// format. The shader order is preserved.
pub fn loadFromFiles(
    alloc_gpa: Allocator,
    paths: configpkg.RepeatablePath,
    target: Target,
) ![]const [:0]const u8 {
    var list: std.ArrayList([:0]const u8) = .empty;
    defer list.deinit(alloc_gpa);
    errdefer for (list.items) |shader| alloc_gpa.free(shader);

    for (paths.value.items) |item| {
        const path, const optional = switch (item) {
            .optional => |path| .{ path, true },
            .required => |path| .{ path, false },
        };

        const shader = loadFromFile(alloc_gpa, path, target) catch |err| {
            if (err == error.FileNotFound and optional) {
                continue;
            }

            return err;
        };
        log.info("loaded custom shader path={s}", .{path});
        try list.append(alloc_gpa, shader);
    }

    return try list.toOwnedSlice(alloc_gpa);
}

/// Load a single shader from a file and convert it to the target language
/// ready to be used with renderers.
pub fn loadFromFile(
    alloc_gpa: Allocator,
    path: []const u8,
    target: Target,
) ![:0]const u8 {
    var arena = ArenaAllocator.init(alloc_gpa);
    defer arena.deinit();
    const alloc = arena.allocator();

    // Read it all into memory -- we don't expect shaders to be large.
    const src = src: {
        // Load the shader file
        const cwd = std.Io.Dir.cwd();
        const file = try cwd.openFile(global.io(), path, .{});
        defer file.close(global.io());
        break :src try compat_file.readToEndAlloc(
            file,
            alloc,
            4 * 1024 * 1024, // 4MB
        );
    };

    // Convert to full GLSL
    const glsl: [:0]const u8 = glsl: {
        var stream: std.Io.Writer.Allocating = .init(alloc);
        try glslFromShader(&stream.writer, src);
        try stream.writer.writeByte(0);
        break :glsl stream.written()[0 .. stream.written().len - 1 :0];
    };

    // Convert to SPIR-V
    const spirv: []const u8 = spirv: {
        var stream: std.Io.Writer.Allocating = .init(alloc);
        var errlog: SpirvLog = .{ .alloc = alloc };
        defer errlog.deinit();
        spirvFromGlsl(&stream.writer, &errlog, glsl) catch |err| {
            if (errlog.info.len > 0 or errlog.debug.len > 0) {
                log.warn("spirv error path={s} info={s} debug={s}", .{
                    path,
                    errlog.info,
                    errlog.debug,
                });
            }

            return err;
        };

        // SpirV pointer must be aligned to 4 bytes since we expect
        // a slice of words.
        var list: std.ArrayListAligned(u8, .of(u32)) = .empty;
        try list.appendSlice(alloc, stream.written());
        break :spirv list.items;
    };

    // Convert to MSL
    return switch (target) {
        // Important: using the alloc_gpa here on purpose because this
        // is the final result that will be returned to the caller.
        .glsl => try glslFromSpv(alloc_gpa, spirv),
        .msl => try mslFromSpv(alloc_gpa, spirv),
    };
}

/// Convert a ShaderToy shader into valid GLSL.
///
/// ShaderToy shaders aren't full shaders, they're just implementing a
/// mainImage function and don't define any of the uniforms. This function
/// will convert the ShaderToy shader into a valid GLSL shader that can be
/// compiled and linked.
pub fn glslFromShader(writer: *std.Io.Writer, src: []const u8) !void {
    const prefix = @embedFile("shaders/shadertoy_prefix.glsl");
    try writer.writeAll(prefix);
    try writer.writeAll("\n\n");
    try writer.writeAll(src);
}

/// Convert a GLSL shader into SPIR-V assembly.
pub fn spirvFromGlsl(
    writer: *std.Io.Writer,
    errlog: ?*SpirvLog,
    src: [:0]const u8,
) !void {
    // So we can run unit tests without fear.
    if (builtin.is_test) try glslang.testing.ensureInit();

    const c = glslang.c;
    const input: c.glslang_input_t = .{
        .language = c.GLSLANG_SOURCE_GLSL,
        .stage = c.GLSLANG_STAGE_FRAGMENT,
        .client = c.GLSLANG_CLIENT_VULKAN,
        .client_version = c.GLSLANG_TARGET_VULKAN_1_2,
        .target_language = c.GLSLANG_TARGET_SPV,
        .target_language_version = c.GLSLANG_TARGET_SPV_1_5,
        .code = src.ptr,
        .default_version = 100,
        .default_profile = c.GLSLANG_NO_PROFILE,
        .force_default_version_and_profile = 0,
        .forward_compatible = 0,
        .messages = c.GLSLANG_MSG_DEFAULT_BIT,
        .resource = c.glslang_default_resource(),
    };

    const shader = try glslang.Shader.create(&input);
    defer shader.delete();

    shader.preprocess(&input) catch |err| {
        if (errlog) |ptr| ptr.fromShader(shader) catch {};
        return err;
    };
    shader.parse(&input) catch |err| {
        if (errlog) |ptr| ptr.fromShader(shader) catch {};
        return err;
    };

    const program = try glslang.Program.create();
    defer program.delete();
    program.addShader(shader);
    program.link(
        c.GLSLANG_MSG_SPV_RULES_BIT |
            c.GLSLANG_MSG_VULKAN_RULES_BIT,
    ) catch |err| {
        if (errlog) |ptr| ptr.fromProgram(program) catch {};
        return err;
    };
    program.spirvGenerate(c.GLSLANG_STAGE_FRAGMENT);
    const size = program.spirvGetSize();
    const ptr = try program.spirvGetPtr();
    const ptr_u8: [*]u8 = @ptrCast(ptr);
    const slice_u8: []u8 = ptr_u8[0 .. size * 4];
    try writer.writeAll(slice_u8);
}

/// Retrieve errors from spirv compilation.
pub const SpirvLog = struct {
    alloc: Allocator,
    info: [:0]const u8 = "",
    debug: [:0]const u8 = "",

    pub fn deinit(self: *const SpirvLog) void {
        if (self.info.len > 0) self.alloc.free(self.info);
        if (self.debug.len > 0) self.alloc.free(self.debug);
    }

    fn fromShader(self: *SpirvLog, shader: *glslang.Shader) !void {
        const info = try shader.getInfoLog();
        const debug = try shader.getDebugInfoLog();
        self.info = "";
        self.debug = "";
        if (info.len > 0) self.info = try self.alloc.dupeSentinel(u8, info, 0);
        if (debug.len > 0) self.debug = try self.alloc.dupeSentinel(u8, debug, 0);
    }

    fn fromProgram(self: *SpirvLog, program: *glslang.Program) !void {
        const info = try program.getInfoLog();
        const debug = try program.getDebugInfoLog();
        self.info = "";
        self.debug = "";
        if (info.len > 0) self.info = try self.alloc.dupeSentinel(u8, info, 0);
        if (debug.len > 0) self.debug = try self.alloc.dupeSentinel(u8, debug, 0);
    }
};

/// Convert SPIR-V binary to MSL.
pub fn mslFromSpv(alloc: Allocator, spv: []const u8) ![:0]const u8 {
    const c = spvcross.c;
    return try spvCross(alloc, spvcross.c.SPVC_BACKEND_MSL, spv, (struct {
        fn setOptions(options: c.spvc_compiler_options) error{SpvcFailed}!void {
            // We enable decoration binding, because we need this
            // to properly locate the uniform block to index 1.
            if (c.spvc_compiler_options_set_bool(
                options,
                c.SPVC_COMPILER_OPTION_MSL_ENABLE_DECORATION_BINDING,
                c.SPVC_TRUE,
            ) != c.SPVC_SUCCESS) {
                return error.SpvcFailed;
            }
        }
    }).setOptions);
}

/// Convert SPIR-V binary to GLSL.
pub fn glslFromSpv(alloc: Allocator, spv: []const u8) ![:0]const u8 {
    const GLSL_VERSION = 430;

    const c = spvcross.c;
    return try spvCross(alloc, c.SPVC_BACKEND_GLSL, spv, (struct {
        fn setOptions(options: c.spvc_compiler_options) error{SpvcFailed}!void {
            if (c.spvc_compiler_options_set_uint(
                options,
                c.SPVC_COMPILER_OPTION_GLSL_VERSION,
                GLSL_VERSION,
            ) != c.SPVC_SUCCESS) {
                return error.SpvcFailed;
            }
        }
    }).setOptions);
}

fn spvCross(
    alloc: Allocator,
    backend: spvcross.c.spvc_backend,
    spv: []const u8,
    comptime optionsFn_: ?*const fn (c: spvcross.c.spvc_compiler_options) error{SpvcFailed}!void,
) ![:0]const u8 {
    // Spir-V is always a multiple of 4 because it is written as a series of words
    if (@mod(spv.len, 4) != 0) return error.SpirvInvalid;

    // Compiler context
    const c = spvcross.c;
    var ctx: c.spvc_context = undefined;
    if (c.spvc_context_create(&ctx) != c.SPVC_SUCCESS) return error.SpvcFailed;
    defer c.spvc_context_destroy(ctx);

    // It would be better to get this out into an output parameter to
    // show users but for now we can just log it.
    c.spvc_context_set_error_callback(ctx, @ptrCast(&(struct {
        fn callback(_: ?*anyopaque, msg_ptr: [*c]const u8) callconv(.c) void {
            const msg = std.mem.sliceTo(msg_ptr, 0);
            std.log.warn("spirv-cross error message={s}", .{msg});
        }
    }).callback), null);

    // Parse the Spir-V binary to an IR
    var ir: c.spvc_parsed_ir = undefined;
    if (c.spvc_context_parse_spirv(
        ctx,
        @ptrCast(@alignCast(spv.ptr)),
        spv.len / 4,
        &ir,
    ) != c.SPVC_SUCCESS) {
        return error.SpvcFailed;
    }

    // Build our compiler to GLSL
    var compiler: c.spvc_compiler = undefined;
    if (c.spvc_context_create_compiler(
        ctx,
        backend,
        ir,
        c.SPVC_CAPTURE_MODE_TAKE_OWNERSHIP,
        &compiler,
    ) != c.SPVC_SUCCESS) {
        return error.SpvcFailed;
    }

    // Setup our options if we have any
    if (optionsFn_) |optionsFn| {
        var options: c.spvc_compiler_options = undefined;
        if (c.spvc_compiler_create_compiler_options(compiler, &options) != c.SPVC_SUCCESS) {
            return error.SpvcFailed;
        }

        try optionsFn(options);

        if (c.spvc_compiler_install_compiler_options(compiler, options) != c.SPVC_SUCCESS) {
            return error.SpvcFailed;
        }
    }

    // Compile the resulting string. This string pointer is owned by the
    // context so we don't need to free it.
    var result: [*:0]const u8 = undefined;
    if (c.spvc_compiler_compile(compiler, @ptrCast(&result)) != c.SPVC_SUCCESS) {
        return error.SpvcFailed;
    }

    return try alloc.dupeSentinel(u8, std.mem.sliceTo(result, 0), 0);
}

/// pepper: how long `custom-shader-animation = cursor` keeps the animation
/// loop running after the cursor or the focus last changed.
pub const cursor_animation_ns: u64 = std.time.ns_per_s;

/// pepper: the monotonic clock (`.awake`) in nanoseconds, for
/// `custom-shader-animation = cursor`'s window. A reading that doesn't fit
/// (it can't) is 0, which never animates.
pub fn monotonicNs() u64 {
    const now: std.Io.Timestamp = .now(global.io(), .awake);
    return std.math.cast(u64, now.nanoseconds) orelse 0;
}

/// pepper: when `custom-shader-animation = cursor`'s window ends, for a
/// change at `now_ns`; none (0) for a clock that couldn't be read (0).
pub fn cursorAnimationEnd(now_ns: u64) u64 {
    if (now_ns == 0) return 0;
    return now_ns +| cursor_animation_ns;
}

/// pepper: whether `custom-shader-animation = cursor` animates at `now_ns`,
/// for a window that ends at `end_ns` (0: none yet). Never for a clock that
/// couldn't be read (0).
pub fn cursorAnimating(end_ns: u64, now_ns: u64) bool {
    return now_ns != 0 and now_ns < end_ns;
}

/// pepper: the cursor glyph's part of `CursorInputs`.
pub const CursorGlyph = struct {
    grid_pos: [2]u16 = .{ 0, 0 },
    glyph_size: [2]u32 = .{ 0, 0 },
    bearings: [2]i16 = .{ 0, 0 },
    color: [4]u8 = .{ 0, 0, 0, 0 },
};

/// pepper: the rest of `CursorInputs`, from the renderer's size.
pub const CursorFrame = struct {
    /// The cell's width and height.
    cell: [2]u32,
    /// The padding's left and top.
    padding: [2]u32,
    /// The screen's height where +Y is up (OpenGL), which the cursor's
    /// position is flipped against; 0 where +Y is down (Metal).
    screen_height: u32,
};

/// pepper: what generic.zig's `updateCustomShaderUniformsForFrame` makes
/// the cursor uniforms from, whose change stamps `iTimeCursorChange`:
/// `custom-shader-animation = cursor` animates for a while after any of it
/// changes. Hiding or showing the cursor, or a new style that keeps the
/// glyph's size (a hollow block for a block), stamps no time a shader could
/// animate from, so it doesn't count (a TUI that hides the cursor around
/// each redraw would otherwise keep the loop running); a bar for a block
/// changes the size, so it does. Seen
/// on the render thread before the loop is set; the uniforms' own stamp
/// starts the window too.
pub const CursorInputs = struct {
    glyph: CursorGlyph,
    frame: CursorFrame,
};

/// pepper: `CursorFrame.screen_height` for a graphics API: the screen's
/// height where +Y is up, which the cursor's pixels are flipped against,
/// and 0 where +Y is down, where a resize alone doesn't move them.
pub fn cursorScreenHeight(y_is_down: bool, height: u32) u32 {
    return if (y_is_down) 0 else height;
}

/// pepper: the cursor inputs now, or null while no cursor glyph is drawn
/// (`glyph` null: blinked off, or hidden): the uniforms keep their last
/// cursor then and stamp nothing, so nothing is seen either, and the next
/// cursor drawn is compared with the last one seen, as the uniforms do.
pub fn cursorInputs(glyph: ?CursorGlyph, frame: CursorFrame) ?CursorInputs {
    return .{ .glyph = glyph orelse return null, .frame = frame };
}

/// Convert ShaderToy shader to null-terminated glsl for testing.
fn testGlslZ(alloc: Allocator, src: []const u8) ![:0]const u8 {
    var buf: std.Io.Writer.Allocating = .init(alloc);
    defer buf.deinit();
    try glslFromShader(&buf.writer, src);
    return try buf.toOwnedSliceSentinel(0);
}

test "custom shader selection uniform layout matches compiled GLSL" {
    const testing = std.testing;
    const c = spvcross.c;
    const src = try testGlslZ(testing.allocator,
        \\void mainImage(out vec4 color, in vec2 pixel) {
        \\    color = vec4(iSelectionForegroundColor + iSelectionBackgroundColor, 1.0);
        \\}
    );
    defer testing.allocator.free(src);

    var buf: [4096 * 4]u8 align(4) = undefined;
    var writer: std.Io.Writer = .fixed(&buf);
    try spirvFromGlsl(&writer, null, src);

    var ctx: c.spvc_context = undefined;
    try testing.expectEqual(c.SPVC_SUCCESS, c.spvc_context_create(&ctx));
    defer c.spvc_context_destroy(ctx);
    var ir: c.spvc_parsed_ir = undefined;
    try testing.expectEqual(c.SPVC_SUCCESS, c.spvc_context_parse_spirv(
        ctx,
        @ptrCast(&buf),
        writer.buffered().len / 4,
        &ir,
    ));
    var compiler: c.spvc_compiler = undefined;
    try testing.expectEqual(c.SPVC_SUCCESS, c.spvc_context_create_compiler(
        ctx,
        c.SPVC_BACKEND_NONE,
        ir,
        c.SPVC_CAPTURE_MODE_TAKE_OWNERSHIP,
        &compiler,
    ));
    var resources: c.spvc_resources = undefined;
    try testing.expectEqual(c.SPVC_SUCCESS, c.spvc_compiler_create_shader_resources(compiler, &resources));
    var blocks: [*c]const c.spvc_reflected_resource = undefined;
    var count: usize = 0;
    try testing.expectEqual(c.SPVC_SUCCESS, c.spvc_resources_get_resource_list_for_type(
        resources,
        c.SPVC_RESOURCE_TYPE_UNIFORM_BUFFER,
        &blocks,
        &count,
    ));
    try testing.expectEqual(@as(usize, 1), count);
    const id = blocks[0].base_type_id;
    const typ = c.spvc_compiler_get_type_handle(compiler, id);

    inline for (.{
        .{ "selection_foreground_color", "iSelectionForegroundColor" },
        .{ "selection_background_color", "iSelectionBackgroundColor" },
    }) |field| {
        var found = false;
        for (0..c.spvc_type_get_num_member_types(typ)) |i| {
            const index: c_uint = @intCast(i);
            const name = std.mem.span(c.spvc_compiler_get_member_name(compiler, id, index));
            if (!std.mem.eql(u8, field[1], name)) continue;
            var offset: c_uint = 0;
            try testing.expectEqual(c.SPVC_SUCCESS, c.spvc_compiler_type_struct_member_offset(compiler, typ, index, &offset));
            try testing.expectEqual(@offsetOf(Uniforms, field[0]), offset);
            found = true;
            break;
        }
        try testing.expect(found);
    }
}

test "spirv" {
    const testing = std.testing;
    const alloc = testing.allocator;

    const src = try testGlslZ(alloc, test_crt);
    defer alloc.free(src);

    var buf: [4096 * 4]u8 = undefined;
    var writer: std.Io.Writer = .fixed(&buf);
    try spirvFromGlsl(&writer, null, src);
}

test "spirv invalid" {
    const testing = std.testing;
    const alloc = testing.allocator;

    const src = try testGlslZ(alloc, test_invalid);
    defer alloc.free(src);

    var buf: [4096 * 4]u8 = undefined;
    var writer: std.Io.Writer = .fixed(&buf);

    var errlog: SpirvLog = .{ .alloc = alloc };
    defer errlog.deinit();
    try testing.expectError(error.GlslangFailed, spirvFromGlsl(&writer, &errlog, src));
    try testing.expect(errlog.info.len > 0);
}

test "shadertoy to msl" {
    const testing = std.testing;
    const alloc = testing.allocator;

    const src = try testGlslZ(alloc, test_crt);
    defer alloc.free(src);

    var buf: std.Io.Writer.Allocating = .init(alloc);
    defer buf.deinit();
    try spirvFromGlsl(&buf.writer, null, src);

    // TODO: Replace this with an aligned version of Writer.Allocating
    var spvlist: std.ArrayListAligned(u8, .of(u32)) = .empty;
    defer spvlist.deinit(alloc);
    try spvlist.appendSlice(alloc, buf.written());

    const msl = try mslFromSpv(alloc, spvlist.items);
    defer alloc.free(msl);
}

test "shadertoy to glsl" {
    const testing = std.testing;
    const alloc = testing.allocator;

    const src = try testGlslZ(alloc, test_crt);
    defer alloc.free(src);

    var buf: std.Io.Writer.Allocating = .init(alloc);
    defer buf.deinit();
    try spirvFromGlsl(&buf.writer, null, src);

    // TODO: Replace this with an aligned version of Writer.Allocating
    var spvlist: std.ArrayListAligned(u8, .of(u32)) = .empty;
    defer spvlist.deinit(alloc);
    try spvlist.appendSlice(alloc, buf.written());

    const glsl = try glslFromSpv(alloc, spvlist.items);
    defer alloc.free(glsl);

    // log.warn("glsl={s}", .{glsl});
}

test "cursorAnimating: for a second after a change, never before one" {
    const testing = std.testing;
    const at: u64 = 5 * std.time.ns_per_s;
    const end = cursorAnimationEnd(at);

    // No change yet (0): no animation, whenever it is.
    try testing.expect(!cursorAnimating(0, 0));
    try testing.expect(!cursorAnimating(0, at));
    // From the change itself up to just before the second is out.
    try testing.expect(cursorAnimating(end, at));
    try testing.expect(cursorAnimating(end, at + std.time.ns_per_ms));
    try testing.expect(cursorAnimating(end, at + cursor_animation_ns - 1));
    // From the second on, until the next change.
    try testing.expect(!cursorAnimating(end, at + cursor_animation_ns));
    try testing.expect(!cursorAnimating(end, at + 3600 * std.time.ns_per_s));
    // A change at the clock's very end doesn't wrap around to the past.
    try testing.expectEqual(std.math.maxInt(u64), cursorAnimationEnd(std.math.maxInt(u64) - 1));
    // A clock that couldn't be read (0) never starts or keeps a window.
    try testing.expectEqual(@as(u64, 0), cursorAnimationEnd(0));
    try testing.expect(!cursorAnimating(end, 0));
    // The clock reads as a real time (not 0, which means no change).
    try testing.expect(monotonicNs() > 0);
}

test "cursorInputs: what starts custom-shader-animation = cursor" {
    const testing = std.testing;
    const glyph: CursorGlyph = .{
        .grid_pos = .{ 3, 4 },
        .glyph_size = .{ 18, 40 },
        .bearings = .{ 0, 38 },
        .color = .{ 255, 255, 255, 255 },
    };
    const frame: CursorFrame = .{
        .cell = .{ 18, 42 },
        .padding = .{ 4, 4 },
        .screen_height = 0,
    };
    const changed = struct {
        /// As generic.zig's `noteCustomShaderCursor`: a frame without a
        /// glyph is skipped, and the next one is compared with the last
        /// one seen.
        fn by(last: CursorInputs, g: ?CursorGlyph, f: CursorFrame) bool {
            const now = cursorInputs(g, f) orelse return false;
            return !std.meta.eql(last, now);
        }
    }.by;
    const first = cursorInputs(glyph, frame).?;

    // The same cursor in the same frame: no change.
    try testing.expect(!changed(first, glyph, frame));
    // No glyph drawn (a blink, or hidden with DECTCEM): nothing seen, and
    // shown again where it was, no change, so a TUI hiding the cursor
    // around each redraw doesn't keep the loop running.
    try testing.expect(cursorInputs(null, frame) == null);
    try testing.expect(!changed(first, null, frame));
    // Hidden while the padding changed, then shown: the uniforms see it
    // moved, and so does this.
    var padded = frame;
    padded.padding = .{ 10, 4 };
    try testing.expect(!changed(first, null, padded));
    try testing.expect(changed(first, glyph, padded));

    // Moved, recolored, or another size (a bar).
    var moved = glyph;
    moved.grid_pos = .{ 4, 4 };
    try testing.expect(changed(first, moved, frame));
    var recolored = glyph;
    recolored.color = .{ 255, 0, 0, 255 };
    try testing.expect(changed(first, recolored, frame));
    var bar = glyph;
    bar.glyph_size = .{ 2, 40 };
    try testing.expect(changed(first, bar, frame));

    // The cell size moves the cursor's pixels: a change.
    var bigger = frame;
    bigger.cell = .{ 20, 46 };
    try testing.expect(changed(first, glyph, bigger));
    // The screen's height counts only where +Y is up (OpenGL: the cursor's
    // pixels are flipped against it), so a window resize alone is a change
    // there and not where +Y is down (Metal).
    inline for (.{ false, true }) |y_is_down| {
        var short = frame;
        short.screen_height = cursorScreenHeight(y_is_down, 900);
        var tall = frame;
        tall.screen_height = cursorScreenHeight(y_is_down, 1000);
        const before = cursorInputs(glyph, short).?;
        try testing.expectEqual(!y_is_down, changed(before, glyph, tall));
    }
}

const test_crt = @embedFile("shaders/test_shadertoy_crt.glsl");
const test_invalid = @embedFile("shaders/test_shadertoy_invalid.glsl");
const test_focus = @embedFile("shaders/test_shadertoy_focus.glsl");
