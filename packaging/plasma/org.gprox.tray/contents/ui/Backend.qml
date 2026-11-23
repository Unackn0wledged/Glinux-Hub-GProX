// gproxd state for the widget. Talks to the daemon through the `gprox` CLI
// (`gprox --json …` prints the daemon's status after each command), run by
// Plasma's executable data engine: QML has no Unix-socket client.
import QtQuick
import org.kde.plasma.plasma5support as P5Support

Item {
    id: backend
    visible: false

    // The CLI to run (Plasmoid.configuration.gproxCommand).
    property string command: "gprox"
    // Last status from gproxd (Status in gprox-core's control.rs), or null.
    property var status: null
    // Why there is no status (gproxd not running, gprox missing, …).
    property string error: ""
    readonly property bool daemonUp: status !== null
    readonly property bool daemonDown: status === null && error !== ""
    // The CLI ran but found no daemon (as opposed to gprox itself failing).
    readonly property bool daemonUnreachable: daemonDown && error.indexOf("cannot reach gproxd") >= 0
    readonly property var headset: status ? status.headset : null
    readonly property bool dongle: !!(headset && headset.dongle)
    readonly property bool connected: !!(headset && headset.connected)
    readonly property var battery: connected ? headset.battery : null
    readonly property var batteryPercent: battery && battery.percent !== null && battery.percent !== undefined
        ? battery.percent : null
    readonly property bool charging: !!(battery && battery.charging)
    readonly property bool micMuted: !!(connected && headset.mic_muted === true)
    readonly property string summary: summaryLine()

    // A write failed; the message is for an inline error banner.
    signal commandFailed(string message)

    // Bumped on every write so that a status poll started before it can't
    // overwrite the newer state the write returns.
    property int writeGen: 0
    property var jobs: ({})

    function quote(s) {
        return "'" + String(s).replace(/'/g, "'\\''") + "'";
    }

    function gprox(args, write) {
        if (write) {
            writeGen++;
        }
        const cmd = [command, "--json"].concat(args).map(quote).join(" ");
        if (jobs[cmd] !== undefined) {
            return; // the same command is still running
        }
        jobs[cmd] = { kind: write ? "write" : "poll", gen: writeGen };
        runner.connectSource(cmd);
    }

    // Run a program detached (gprox-gui, systemctl), not waiting for it.
    function launch(args) {
        const cmd = "setsid -f " + args.map(quote).join(" ") + " >/dev/null 2>&1";
        jobs[cmd] = { kind: "launch", gen: writeGen };
        runner.connectSource(cmd);
    }

    function refresh() { gprox(["status"], false); }
    function setHpEq(name) { gprox(["preset", "set", name], true); }
    function setBluevoice(name) { gprox(["bluevoice", "set", name], true); }
    function bluevoiceEnable(on) { gprox(["bluevoice", on ? "on" : "off"], true); }
    function setSidetone(level) { gprox(["sidetone", String(Math.round(level))], true); }
    function setAlwaysOnSidetone(on) { gprox(["sidetone", "always-on", on ? "on" : "off"], true); }
    function setAutoOff(minutes) { gprox(["auto-off", minutes === 0 ? "never" : String(minutes)], true); }

    function handle(job, data) {
        if (job.kind === "launch") {
            return;
        }
        const out = (data["stdout"] || "").trim();
        let parsed = null;
        try {
            parsed = out ? JSON.parse(out) : null;
        } catch (e) {
            parsed = null;
        }
        if (parsed && parsed.hp_eq) {
            if (job.kind === "poll" && job.gen !== writeGen) {
                return; // stale: a write happened since this poll started
            }
            status = parsed;
            error = "";
            return;
        }
        let msg = parsed && parsed.error ? parsed.error : (data["stderr"] || "").trim();
        if (!msg) {
            msg = "gprox exited with code " + data["exit code"];
        }
        msg = msg.replace(/^gprox: /, "");
        if (job.kind === "poll") {
            status = null;
            error = msg;
        } else {
            commandFailed(msg);
            refresh(); // show the real state again
        }
    }

    // HeadsetStatus::summary() from gprox-core, for the tooltip.
    function summaryLine() {
        if (!status) {
            return error ? error : "Waiting for gproxd…";
        }
        const h = status.headset || {};
        if (!h.dongle) {
            return h.error ? h.error : "dongle not found";
        }
        if (!h.connected) {
            return "off or out of range";
        }
        const parts = ["on"];
        const b = h.battery;
        if (b) {
            let s = b.percent !== null && b.percent !== undefined
                ? "battery " + b.percent + " % (" + b.millivolts + " mV"
                : "battery " + b.millivolts + " mV";
            if (b.charging) {
                s += ", charging";
            } else if (b.hours_left !== null && b.hours_left !== undefined) {
                s += ", ~" + Math.round(b.hours_left) + " h left";
            }
            if (b.percent !== null && b.percent !== undefined) {
                s += ")";
            }
            parts.push(s);
        }
        const always = !h.always_on_sidetone ? ""
            : h.mic_kept_open ? " (always on)" : " (always on, mic stream not open yet)";
        if (h.sidetone !== null && h.sidetone !== undefined) {
            parts.push("sidetone " + h.sidetone + always);
        } else if (h.always_on_sidetone) {
            parts.push("sidetone" + always);
        }
        if (h.auto_off_minutes === 0) {
            parts.push("auto-off never");
        } else if (h.auto_off_minutes !== null && h.auto_off_minutes !== undefined) {
            parts.push("auto-off " + h.auto_off_minutes + " min");
        }
        if (h.mic_muted === true) {
            parts.push("mic muted");
        } else if (h.mic_muted === false) {
            parts.push("mic live");
        }
        return parts.join(", ");
    }

    P5Support.DataSource {
        id: runner
        engine: "executable"
        connectedSources: []
        onNewData: (source, data) => {
            const job = backend.jobs[source];
            delete backend.jobs[source];
            disconnectSource(source);
            if (job !== undefined) {
                backend.handle(job, data);
            }
        }
    }
}
