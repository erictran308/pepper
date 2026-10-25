const std = @import("std");

const Parser = @import("../../osc.zig").Parser;
const Command = @import("../../osc.zig").Command;

const log = std.log.scoped(.osc_rxvt_extension);

/// Parse OSC 777
pub fn parse(parser: *Parser, _: ?u8) ?*Command {
    const cap = if (parser.capture) |*c| c else {
        parser.state = .invalid;
        return null;
    };
    // ensure that we are sentinel terminated
    cap.writer.writeByte(0) catch {
        parser.state = .invalid;
        return null;
    };
    const data = cap.trailing();
    const k = std.mem.indexOfScalar(u8, data, ';') orelse {
        parser.state = .invalid;
        return null;
    };
    const ext = data[0..k];
    if (std.mem.eql(u8, ext, "pepper")) return parsePepper(parser, data[k + 1 ..]);
    if (!std.mem.eql(u8, ext, "notify")) {
        log.warn("unknown rxvt extension: {s}", .{ext});
        parser.state = .invalid;
        return null;
    }
    const t = std.mem.indexOfScalarPos(u8, data, k + 1, ';') orelse {
        log.warn("rxvt notify extension is missing the title", .{});
        parser.state = .invalid;
        return null;
    };
    data[t] = 0;
    const title = data[k + 1 .. t :0];
    const body = data[t + 1 .. data.len - 1 :0];
    parser.command = .{
        .show_desktop_notification = .{
            .title = title,
            .body = body,
        },
    };
    return &parser.command;
}

/// pepper: `777;pepper;place;TOKEN;COL;ROW;COLS;ROWS` puts the video stream
/// with that token over those cells, and `777;pepper;remove;TOKEN` takes it
/// away. A place can end in up to 4 holes, `;COL,ROW,COLS,ROWS` each: cells
/// (from the top left of the screen, as the box) where the stream doesn't
/// show, so a popup drawn there does. A place whose holes don't read takes
/// the box away: shown whole, the video would cover what they were for.
/// `data` is what follows `pepper;`, ending in the 0 written above.
fn parsePepper(parser: *Parser, data: []u8) ?*Command {
    const rest = data[0 .. data.len - 1];
    const op_end = std.mem.indexOfScalar(u8, rest, ';') orelse
        return invalidPepper(parser, "missing token");
    const op = rest[0..op_end];

    const token_start = op_end + 1;
    const token_end = std.mem.indexOfScalarPos(u8, rest, token_start, ';') orelse rest.len;
    const token_bytes = rest[token_start..token_end];
    if (token_bytes.len == 0 or token_bytes.len > Command.PepperPlace.token_max)
        return invalidPepper(parser, "bad token length");
    for (token_bytes) |c| {
        if (!std.ascii.isAlphanumeric(c) and c != '-' and c != '_')
            return invalidPepper(parser, "bad token character");
    }

    var cells: [4]u16 = @splat(0);
    var holes: []const u8 = "";
    if (std.mem.eql(u8, op, "place")) {
        if (token_end == rest.len) return invalidPepper(parser, "missing cells");
        var it = std.mem.splitScalar(u8, rest[token_end + 1 ..], ';');
        for (&cells) |*v| {
            const s = it.next() orelse return invalidPepper(parser, "missing cells");
            v.* = Command.PepperPlace.cellCount(s) orelse return invalidPepper(parser, "bad cell");
        }
        if (cells[2] == 0 or cells[3] == 0) return invalidPepper(parser, "empty box");
        if (it.peek() != null) {
            holes = it.rest();
            if (holes.len == 0 or Command.PepperPlace.readHoles(holes) == null) {
                log.warn("invalid pepper extension: bad holes, so the box is taken away", .{});
                holes = "";
                cells = @splat(0);
            }
        }
    } else if (std.mem.eql(u8, op, "remove")) {
        if (token_end != rest.len) return invalidPepper(parser, "too many fields");
    } else {
        return invalidPepper(parser, "unknown operation");
    }

    data[token_end] = 0;
    parser.command = .{
        .pepper_place = .{
            .token = data[token_start..token_end :0],
            .col = cells[0],
            .row = cells[1],
            .cols = cells[2],
            .rows = cells[3],
            .holes = holes,
        },
    };
    return &parser.command;
}

fn invalidPepper(parser: *Parser, reason: []const u8) ?*Command {
    log.warn("invalid pepper extension: {s}", .{reason});
    parser.state = .invalid;
    return null;
}

test "OSC: OSC 777 pepper place" {
    const testing = std.testing;

    var p: Parser = .init(null);

    const input = "777;pepper;place;a1B2-c_3;4;5;60;17";
    for (input) |ch| p.next(ch);

    const cmd = p.end('\x1b').?.*;
    try testing.expect(cmd == .pepper_place);
    try testing.expectEqualStrings("a1B2-c_3", cmd.pepper_place.token);
    try testing.expectEqual(4, cmd.pepper_place.col);
    try testing.expectEqual(5, cmd.pepper_place.row);
    try testing.expectEqual(60, cmd.pepper_place.cols);
    try testing.expectEqual(17, cmd.pepper_place.rows);
}

test "OSC: OSC 777 pepper place, a 64-byte token" {
    const testing = std.testing;

    var p: Parser = .init(null);

    const token = "t" ** 64;
    const input = "777;pepper;place;" ++ token ++ ";0;0;1;1";
    for (input) |ch| p.next(ch);

    const cmd = p.end('\x1b').?.*;
    try testing.expect(cmd == .pepper_place);
    try testing.expectEqualStrings(token, cmd.pepper_place.token);
    try testing.expectEqualStrings("", cmd.pepper_place.holes);
}

test "OSC: OSC 777 pepper place with holes" {
    const testing = std.testing;

    var p: Parser = .init(null);

    const input = "777;pepper;place;tok;4;5;60;17;10,12,30,8;0,0,65535,1";
    for (input) |ch| p.next(ch);

    const cmd = p.end('\x1b').?.*;
    try testing.expect(cmd == .pepper_place);
    try testing.expectEqualStrings("tok", cmd.pepper_place.token);
    try testing.expectEqual(17, cmd.pepper_place.rows);
    const holes = Command.PepperPlace.readHoles(cmd.pepper_place.holes).?;
    try testing.expectEqual(2, holes.len);
    try testing.expectEqual(
        Command.PepperPlace.Cells{ .col = 10, .row = 12, .cols = 30, .rows = 8 },
        holes.slice()[0],
    );
    try testing.expectEqual(
        Command.PepperPlace.Cells{ .col = 0, .row = 0, .cols = 65535, .rows = 1 },
        holes.slice()[1],
    );
}

test "OSC: OSC 777 pepper place with the most holes" {
    const testing = std.testing;

    var p: Parser = .init(null);

    const input = "777;pepper;place;tok;0;0;9;9;1,1,1,1;2,2,1,1;3,3,1,1;4,4,1,1";
    for (input) |ch| p.next(ch);

    const cmd = p.end('\x1b').?.*;
    try testing.expect(cmd == .pepper_place);
    const holes = Command.PepperPlace.readHoles(cmd.pepper_place.holes).?;
    try testing.expectEqual(Command.PepperPlace.holes_max, holes.len);
    try testing.expectEqual(4, holes.slice()[3].row);
}

test "OSC: OSC 777 pepper remove" {
    const testing = std.testing;

    var p: Parser = .init(null);

    const input = "777;pepper;remove;a1B2";
    for (input) |ch| p.next(ch);

    const cmd = p.end('\x1b').?.*;
    try testing.expect(cmd == .pepper_place);
    try testing.expectEqualStrings("a1B2", cmd.pepper_place.token);
    try testing.expectEqual(0, cmd.pepper_place.cols);
    try testing.expectEqual(0, cmd.pepper_place.rows);
}

test "OSC: OSC 777 pepper place with bad holes takes the box away" {
    const testing = std.testing;

    const inputs = [_][]const u8{
        "777;pepper;place;tok;1;2;3;4;",
        "777;pepper;place;tok;1;2;3;4;5",
        "777;pepper;place;tok;1;2;3;4;5,6,7",
        "777;pepper;place;tok;1;2;3;4;5,6,7,8,9",
        "777;pepper;place;tok;1;2;3;4;5,6,0,8",
        "777;pepper;place;tok;1;2;3;4;5,6,7,0",
        "777;pepper;place;tok;1;2;3;4;5,6,7,8;",
        "777;pepper;place;tok;1;2;3;4;5,6,7,8;;5,6,7,8",
        "777;pepper;place;tok;1;2;3;4;5,,7,8",
        "777;pepper;place;tok;1;2;3;4; 5,6,7,8",
        "777;pepper;place;tok;1;2;3;4;+5,6,7,8",
        "777;pepper;place;tok;1;2;3;4;5,6,7,70000",
        "777;pepper;place;tok;1;2;3;4;5;6;7;8",
        "777;pepper;place;tok;1;2;3;4;1,1,1,1;1,1,1,1;1,1,1,1;1,1,1,1;1,1,1,1",
    };
    for (inputs) |input| {
        var p: Parser = .init(null);
        for (input) |ch| p.next(ch);
        const cmd = p.end('\x1b').?.*;
        try testing.expect(cmd == .pepper_place);
        try testing.expectEqualStrings("tok", cmd.pepper_place.token);
        try testing.expectEqual(0, cmd.pepper_place.cols);
        try testing.expectEqual(0, cmd.pepper_place.rows);
        try testing.expectEqualStrings("", cmd.pepper_place.holes);
    }
}

test "OSC: OSC 777 pepper invalid" {
    const testing = std.testing;

    const inputs = [_][]const u8{
        "777;pepper",
        "777;pepper;place",
        "777;pepper;place;tok",
        "777;pepper;place;tok;1;2;3",
        "777;pepper;place;tok;1;2;0;4",
        "777;pepper;place;tok;1;2;3;70000",
        "777;pepper;place;tok;+1;2;3;4",
        "777;pepper;place;tok;-0;2;3;4",
        "777;pepper;place;tok;1_0;2;3;4",
        "777;pepper;place;tok;1;2; 3;4",
        "777;pepper;place;tok;1;;3;4",
        "777;pepper;place;tok;000001;2;3;4",
        "777;pepper;place;to;k;1;2;3;4",
        "777;pepper;place;to k;1;2;3;4",
        "777;pepper;place;;1;2;3;4",
        "777;pepper;remove;tok;1",
        "777;pepper;move;tok",
        "777;pepper;remove;" ++ "x" ** 65,
        "777;pepper;place;tok;1;2;0;4;5,6,7,8",
    };
    for (inputs) |input| {
        var p: Parser = .init(null);
        for (input) |ch| p.next(ch);
        try testing.expect(p.end('\x1b') == null);
    }
}

test "OSC: OSC 777 show desktop notification with title" {
    const testing = std.testing;

    var p: Parser = .init(null);

    const input = "777;notify;Title;Body";
    for (input) |ch| p.next(ch);

    const cmd = p.end('\x1b').?.*;
    try testing.expect(cmd == .show_desktop_notification);
    try testing.expectEqualStrings(cmd.show_desktop_notification.title, "Title");
    try testing.expectEqualStrings(cmd.show_desktop_notification.body, "Body");
}
