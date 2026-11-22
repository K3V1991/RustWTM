namespace Wtm.Hardware;

// The provider owns the final control lease. A service crash, closed input or
// missed renewals causes an attempt to return each channel to hardware default.
internal sealed class Leases(Func<long> clock)
{
    private sealed record Entry(Action Restore, long Expires);
    private readonly Dictionary<string, Entry> entries = [];

    public void Applied(string id, Action restore)
    {
        // A refresh can report a replacement control under the same identifier.
        // Its lease must start only after the previous control is restored.
        if (entries.TryGetValue(id, out var previous)
            && (previous.Restore != restore || clock() >= previous.Expires))
            RestoreEntry(id, previous);
        entries[id] = new(restore, clock() + 8000);
    }

    public void Apply(string id, Action restore, Action write)
    {
        Applied(id, restore);
        try { write(); }
        catch
        {
            // A partial write may have changed the fan. If restoration fails, retry on the next watchdog pass.
            var due = entries[id] with { Expires = clock() };
            entries[id] = due;
            Restore(id, due);
            throw;
        }
    }

    public bool RestoreApplied(string id)
    {
        if (!entries.TryGetValue(id, out var entry)) return false;
        RestoreEntry(id, entry);
        return true;
    }

    private void RestoreEntry(string id, Entry entry)
    {
        entry.Restore();
        entries.Remove(id);
    }

    public void Expire()
    {
        foreach (var (id, entry) in entries.ToArray())
            if (clock() >= entry.Expires) Restore(id, entry);
    }
    private void Restore(string id, Entry entry)
    {
        // Keep a failed rollback so a later watchdog pass can retry it.
        try { RestoreEntry(id, entry); }
        catch (Exception e) { Console.Error.WriteLine($"Restore {id}: {e.Message}"); }
    }
    public bool RestoreAll()
    {
        foreach (var (id, entry) in entries.ToArray()) Restore(id, entry);
        return entries.Count == 0;
    }
    // The watchdog is stopping. Retry only the leases that failed the first pass.
    public bool RestoreOnExit() => RestoreAll() || RestoreAll();

    public int Count => entries.Count;

    public static void Test()
    {
        static void Check(bool condition) { if (!condition) throw new Exception("Lease assertion failed"); }
        long time = 0; int restored = 0;
        var lease = new Leases(() => time);
        Action restoreA = () => restored++;
        lease.Applied("a", restoreA);
        time = 7999; lease.Expire(); Check(restored == 0);
        lease.Applied("a", restoreA); // A renewal extends only this channel.
        time = 8000; lease.Expire(); Check(restored == 0);
        time = 15999; lease.Expire(); Check(restored == 1 && lease.Count == 0);
        bool fail = true;
        lease.Applied("b", () => { if (fail) throw new Exception("injected restore failure"); restored++; });
        Check(!lease.RestoreAll() && lease.Count == 1);
        fail = false; Check(lease.RestoreAll() && restored == 2);
        int healthyAttempts = 0;
        int transientAttempts = 0;
        lease.Applied("healthy", () => healthyAttempts++);
        lease.Applied("transient", () =>
        {
            if (++transientAttempts == 1) throw new Exception("injected transient restore failure");
        });
        Check(lease.RestoreOnExit() && lease.Count == 0 && healthyAttempts == 1 && transientAttempts == 2);
        int persistentAttempts = 0;
        lease.Applied("persistent", () =>
        {
            persistentAttempts++;
            throw new Exception("injected persistent restore failure");
        });
        Check(!lease.RestoreOnExit() && lease.Count == 1 && persistentAttempts == 2);
        var handoff = new Leases(() => time);
        var restoreOrder = new List<string>();
        Action restorePrevious = () => restoreOrder.Add("previous");
        Action restoreReplacement = () => restoreOrder.Add("replacement");
        handoff.Applied("fan", restorePrevious);
        handoff.Applied("fan", restoreReplacement);
        Check(restoreOrder.SequenceEqual(new[] { "previous" }));
        Check(handoff.RestoreApplied("fan") && handoff.Count == 0);
        Check(restoreOrder.SequenceEqual(new[] { "previous", "replacement" }));
        Check(!handoff.RestoreApplied("fan"));

        int previousAttempts = 0;
        handoff.Applied("fan", () =>
        {
            if (++previousAttempts == 1) throw new InvalidOperationException("injected old control failure");
        });
        bool handoffRefused = false;
        try { handoff.Applied("fan", restoreReplacement); }
        catch (InvalidOperationException) { handoffRefused = true; }
        Check(handoffRefused && handoff.Count == 1);
        Check(handoff.RestoreApplied("fan") && previousAttempts == 2 && handoff.Count == 0);
        Check(restoreOrder.Count == 2);
        TestFailedWriteRestoration();
        TestExpiredRenewalRestoration();
        Program.ValidateDuty(30,30,100); Program.ValidateDuty(100,30,100);
        foreach (float value in new[] {float.NaN,float.PositiveInfinity,29,101})
        {
            bool rejected=false;try {Program.ValidateDuty(value,30,100);}catch(ArgumentOutOfRangeException){rejected=true;}
            Check(rejected);
        }
        Check(Program.ReadingValue(LibreHardwareMonitor.Hardware.HardwareType.Cpu, LibreHardwareMonitor.Hardware.SensorType.Temperature, 0, false) is null);
        Check(Program.ReadingValue(LibreHardwareMonitor.Hardware.HardwareType.GpuNvidia, LibreHardwareMonitor.Hardware.SensorType.Fan, 0, false) == 0);
        Check(Program.ReadingValue(LibreHardwareMonitor.Hardware.HardwareType.GpuNvidia, LibreHardwareMonitor.Hardware.SensorType.Temperature, float.NaN, false) is null);
        Console.WriteLine("PASS: lease deadlines, renewal, expiry, rollback retry, shutdown, duty bounds and unavailable readings");
    }

    private static void TestFailedWriteRestoration()
    {
        var lease = new Leases(() => 0);
        bool manual = false;
        const string writeError = "injected partial write";
        bool writeFailed = false;
        try
        {
            lease.Apply("fan", () => manual = false, () =>
            {
                manual = true;
                throw new InvalidOperationException(writeError);
            });
        }
        catch (InvalidOperationException e) when (e.Message == writeError) { writeFailed = true; }
        if (!writeFailed || manual || lease.Count != 0)
            throw new Exception("Failed write left the fan in software control");
    }

    private static void TestExpiredRenewalRestoration()
    {
        long time = 0;
        var lease = new Leases(() => time);
        int writes = 0;
        bool failRestore = true;
        Action restore = () =>
        {
            if (failRestore) throw new InvalidOperationException("injected expiry restore failure");
        };
        Action write = () => writes++;
        lease.Apply("fan", restore, write);
        time = 8000;
        bool renewalRefused = false;
        try { lease.Apply("fan", restore, write); }
        catch (InvalidOperationException) { renewalRefused = true; }
        if (!renewalRefused || writes != 1 || lease.Count != 1)
            throw new Exception("An expired lease accepted a write before restoring automatic control");
        failRestore = false;
        lease.Expire();
        if (lease.Count != 0)
            throw new Exception("An expired lease was not restored when its retry succeeded");
    }
}
