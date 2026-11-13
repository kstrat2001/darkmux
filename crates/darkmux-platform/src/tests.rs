use super::*;

#[test]
fn macos_reaches_the_host_through_docker_desktops_builtin_name() {
    assert_eq!(
        MacPlatform.container_host_access(),
        ContainerHostAccess::Builtin
    );
}

#[test]
fn linux_maps_the_host_gateway_itself() {
    assert_eq!(
        LinuxPlatform.container_host_access(),
        ContainerHostAccess::HostGateway
    );
}

#[test]
fn an_os_with_no_strategy_of_its_own_changes_nothing() {
    assert_eq!(
        OtherPlatform.container_host_access(),
        ContainerHostAccess::Builtin
    );
}

/// `current()` is the one place an OS is named: it must hand back the
/// strategy for the OS this binary was built for.
#[test]
fn current_is_the_strategy_for_the_build_target() {
    let expected = match std::env::consts::OS {
        "macos" => "macos",
        "linux" => "linux",
        _ => "other",
    };
    assert_eq!(current().id(), expected);
}

#[test]
fn a_fake_answers_what_the_test_set() {
    let fake = FakePlatform {
        container_host_access: ContainerHostAccess::HostGateway,
        container_user: ContainerUser::Image,
        machine_uid: None,
        ..FakePlatform::default()
    };
    assert_eq!(fake.id(), "fake");
    assert_eq!(
        fake.container_host_access(),
        ContainerHostAccess::HostGateway
    );
}

#[test]
fn macos_and_other_run_the_container_as_the_images_own_user() {
    assert_eq!(MacPlatform.container_user(), ContainerUser::Image);
    assert_eq!(OtherPlatform.container_user(), ContainerUser::Image);
}

/// On Linux the bind mounts keep the host's ownership, so the container runs
/// as the host user; off Linux there is no `/proc` to read and the answer
/// falls back to the image's user rather than guessing.
#[test]
fn linux_runs_the_container_as_the_host_user() {
    let answer = LinuxPlatform.container_user();
    if std::env::consts::OS == "linux" {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let (uid, gid) = crate::linux::ids_from_status(&status).unwrap();
        assert_eq!(
            answer,
            ContainerUser::Host {
                uid,
                gid: Some(gid)
            }
        );
    } else {
        assert_eq!(answer, ContainerUser::Image);
    }
}

#[test]
fn the_effective_ids_come_from_proc_status() {
    let status = "Name:\tdarkmux\nUid:\t1001\t1002\t1003\t1004\nGid:\t2001\t2002\t2003\t2004\n";
    assert_eq!(crate::linux::ids_from_status(status), Some((1002, 2002)));
    assert_eq!(crate::linux::ids_from_status("Name:\tx\n"), None);
}

#[test]
fn an_override_names_the_image_user_or_a_numeric_id() {
    assert_eq!(ContainerUser::parse("image"), Ok(ContainerUser::Image));
    assert_eq!(
        ContainerUser::parse(" 1001 "),
        Ok(ContainerUser::Host {
            uid: 1001,
            gid: None
        })
    );
    assert_eq!(
        ContainerUser::parse("1001:1002"),
        Ok(ContainerUser::Host {
            uid: 1001,
            gid: Some(1002)
        })
    );
    for bad in ["", "root", "1001:", ":1001", "1001:x", "-1", "1001:1002:3"] {
        assert!(
            ContainerUser::parse(bad).is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn a_host_user_is_written_the_way_docker_reads_it() {
    assert_eq!(
        ContainerUser::Host {
            uid: 1001,
            gid: Some(1002)
        }
        .docker_user(),
        Some("1001:1002".to_string())
    );
    assert_eq!(
        ContainerUser::Host {
            uid: 1001,
            gid: None
        }
        .docker_user(),
        Some("1001".to_string())
    );
    assert_eq!(ContainerUser::Image.docker_user(), None);
}

#[test]
fn a_fake_answers_the_container_user_the_test_set() {
    let fake = FakePlatform {
        container_host_access: ContainerHostAccess::Builtin,
        container_user: ContainerUser::Host {
            uid: 5,
            gid: Some(6),
        },
        machine_uid: None,
        ..FakePlatform::default()
    };
    assert_eq!(
        fake.container_user(),
        ContainerUser::Host {
            uid: 5,
            gid: Some(6)
        }
    );
}

/// macOS's identity is the `IOPlatformUUID`, parsed from `ioreg` exactly as
/// `darkmux-hardware` did before this moved here.
#[test]
fn parses_io_platform_uuid_from_sample() {
    let sample = r#"
    "IOPlatformUUID" = "00000000-0000-4000-8000-ABCDEF000009"
    "IOPlatformSerialNumber" = "C02XYZ123"
"#;
    assert_eq!(
        crate::macos::parse_io_platform_uuid(sample).as_deref(),
        Some("00000000-0000-4000-8000-ABCDEF000009")
    );
}

#[test]
fn parse_uuid_none_when_absent() {
    assert_eq!(
        crate::macos::parse_io_platform_uuid("no uuid here\nother stuff"),
        None
    );
    // An empty value must NOT leak as Some("") — the #640 contract hinges
    // on None-vs-name, so an empty identity would be a silent downstream
    // bug (treated as a valid-but-blank machine).
    assert_eq!(
        crate::macos::parse_io_platform_uuid("\"IOPlatformUUID\" = \"\"\n"),
        None
    );
}

/// What would identify a Mac is read from the same `ioreg` output: its
/// serial as well as its UUID, each only when it has a value.
#[test]
fn reads_a_serial_number_beside_the_uuid() {
    let sample = "    \"IOPlatformUUIDExtra\" = \"nope\"\n    \"IOPlatformSerialNumber\" = \"C02XYZ123\"\n";
    assert_eq!(crate::macos::parse_io_platform_value(sample, "IOPlatformSerialNumber").as_deref(), Some("C02XYZ123"));
    assert_eq!(crate::macos::parse_io_platform_value(sample, "IOPlatformUUID"), None);
    assert_eq!(crate::macos::parse_io_platform_value("\"IOPlatformSerialNumber\" = \"  \"", "IOPlatformSerialNumber"), None);
    assert_eq!(crate::macos::parse_io_platform_value("\"IOPlatformSerialNumber\" without a value", "IOPlatformSerialNumber"), None);
}

/// A device-tree string ends in NUL; a command's output in a newline. Neither is part of the value.
#[test]
fn trimmed_drops_nul_and_whitespace_and_reads_empty_as_nothing() {
    assert_eq!(crate::trimmed("C02XYZ123\0").as_deref(), Some("C02XYZ123"));
    assert_eq!(crate::trimmed("  macbook-dm-5\n").as_deref(), Some("macbook-dm-5"));
    assert_eq!(crate::trimmed("\0\n"), None);
}

/// Only a fake that is told what identifies it says so; an OS with no strategy reads nothing.
#[test]
fn identifying_strings_come_from_the_platform() {
    let fake = crate::fake::FakePlatform { identifying_strings: vec!["C02XYZ123".into()], ..Default::default() };
    assert_eq!(fake.identifying_strings(), vec!["C02XYZ123".to_string()]);
    assert!(crate::fake::FakePlatform::default().identifying_strings().is_empty());
    assert!(crate::other::OtherPlatform.identifying_strings().is_empty());
    // Each OS's own reading, wherever the tests run: never a blank or padded value.
    for found in [crate::macos::MacPlatform.identifying_strings(), crate::linux::LinuxPlatform.identifying_strings()] {
        assert!(found.iter().all(|s| crate::trimmed(s).as_deref() == Some(s.as_str())), "{found:?}");
    }
}

/// Linux's identity is the install's `/etc/machine-id`: 32 lowercase hex
/// characters. Anything else is no identity, never a guess.
#[test]
fn a_linux_machine_id_is_32_hex_characters() {
    let id = "4c6f6d6172636879626f6f6b30303031";
    assert_eq!(
        crate::linux::parse_machine_id(&format!("{id}\n")).as_deref(),
        Some(id)
    );
    for bad in [
        "",
        "\n",
        "uninitialized\n",
        "4c6f6d61",
        "4C6F6D6172636879626F6F6B30303031",
        "zz6f6d6172636879626f6f6b30303031",
    ] {
        assert_eq!(crate::linux::parse_machine_id(bad), None, "{bad:?}");
    }
}

#[test]
fn linux_reads_the_installs_machine_id() {
    let answer = LinuxPlatform.machine_uid();
    if std::env::consts::OS == "linux" {
        let raw = std::fs::read_to_string("/etc/machine-id").unwrap();
        assert_eq!(answer, crate::linux::parse_machine_id(&raw));
        assert!(answer.is_some(), "a systemd Linux has a machine id");
    } else {
        assert_eq!(answer, None);
    }
}

#[test]
fn an_os_with_no_strategy_has_no_machine_uid() {
    assert_eq!(OtherPlatform.machine_uid(), None);
}

#[test]
fn a_fake_answers_the_machine_uid_the_test_set() {
    let fake = FakePlatform {
        container_host_access: ContainerHostAccess::Builtin,
        container_user: ContainerUser::Image,
        machine_uid: Some("uid-1".to_string()),
        ..FakePlatform::default()
    };
    assert_eq!(fake.machine_uid().as_deref(), Some("uid-1"));
}

/// Under Asahi the chip comes from the device tree's `compatible`, named the
/// way macOS's `machdep.cpu.brand_string` names it, so a dual-boot Mac reads
/// the same on both sides.
#[test]
fn an_apple_chip_is_named_from_the_device_tree() {
    let m2 = "apple,j493\0apple,t8112\0apple,arm-platform\0";
    assert_eq!(
        crate::linux::apple_chip_from_compatible(m2).as_deref(),
        Some("Apple M2")
    );
    let m1_max = "apple,j316c\0apple,t6001\0apple,arm-platform\0";
    assert_eq!(
        crate::linux::apple_chip_from_compatible(m1_max).as_deref(),
        Some("Apple M1 Max")
    );
    assert_eq!(
        crate::linux::apple_chip_from_compatible("apple,j999\0apple,t9999\0"),
        None,
        "an unknown chip is not guessed"
    );
    assert_eq!(
        crate::linux::apple_chip_from_compatible("raspberrypi,4-model-b\0brcm,bcm2711\0"),
        None
    );
}

#[test]
fn other_linux_names_the_cpu_from_cpuinfo() {
    let cpuinfo = "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz\n";
    assert_eq!(
        crate::linux::model_name_from_cpuinfo(cpuinfo).as_deref(),
        Some("Intel(R) Core(TM) i7-8550U CPU @ 1.80GHz")
    );
    assert_eq!(
        crate::linux::model_name_from_cpuinfo("processor\t: 0\nBogoMIPS\t: 48.00\n"),
        None
    );
}

#[test]
fn macos_names_the_cpu_from_sysctl_and_other_reports_none() {
    assert_eq!(
        crate::macos::parse_brand_string("Apple M5 Max\n").as_deref(),
        Some("Apple M5 Max")
    );
    assert_eq!(crate::macos::parse_brand_string("  \n"), None);
    assert_eq!(OtherPlatform.cpu_brand(), None);
}

#[test]
fn this_linux_machine_names_its_chip() {
    if std::env::consts::OS == "linux"
        && std::path::Path::new("/proc/device-tree/compatible").exists()
    {
        let compatible = std::fs::read_to_string("/proc/device-tree/compatible").unwrap();
        if compatible.contains("apple,") {
            assert_eq!(
                LinuxPlatform.cpu_brand(),
                crate::linux::apple_chip_from_compatible(&compatible)
            );
        }
    }
}

const GIB: u64 = 1024 * 1024 * 1024;

/// Linux reports usable memory (`MemTotal`), less than what is installed:
/// firmware and the kernel keep a share. Rounded UP to a whole GiB, it reads
/// as the machine is sold, the way macOS's `hw.memsize` does.
#[test]
fn linux_installed_ram_rounds_memtotal_up_to_a_whole_gib() {
    let m2_8gb = "MemTotal:        7715696 kB\nMemFree:          823140 kB\n";
    assert_eq!(
        crate::linux::installed_ram_from_meminfo(m2_8gb),
        Some(8 * GIB)
    );
    let exact = format!("MemTotal: {} kB\n", 16 * 1024 * 1024);
    assert_eq!(
        crate::linux::installed_ram_from_meminfo(&exact),
        Some(16 * GIB),
        "a whole GiB stays put"
    );
    assert_eq!(
        crate::linux::installed_ram_from_meminfo("MemFree: 1 kB\n"),
        None
    );
}

#[test]
fn macos_installed_ram_is_hw_memsize() {
    assert_eq!(crate::macos::parse_memsize("8589934592\n"), Some(8 * GIB));
    assert_eq!(crate::macos::parse_memsize("not a number"), None);
    assert_eq!(OtherPlatform.ram_installed_bytes(), None);
}

#[test]
fn this_linux_machine_reports_installed_ram() {
    if std::env::consts::OS == "linux" {
        let ram = LinuxPlatform.ram_installed_bytes().expect("/proc/meminfo");
        assert_eq!(ram % GIB, 0, "a whole GiB");
    }
}

/// (#3277 review) Without systemd's `/etc/machine-id`, D-Bus's copy is read;
/// an invalid first file is skipped, not taken.
#[test]
fn the_machine_id_falls_back_to_the_dbus_copy() {
    let dir = tempfile::tempdir().unwrap();
    let (etc, dbus) = (dir.path().join("etc-machine-id"), dir.path().join("dbus-machine-id"));
    let id = "0123456789abcdef0123456789abcdef";
    std::fs::write(&dbus, format!("{id}\n")).unwrap();
    assert_eq!(crate::linux::first_machine_id(&[&etc, &dbus]).as_deref(), Some(id), "etc missing");
    std::fs::write(&etc, "uninitialized\n").unwrap();
    assert_eq!(crate::linux::first_machine_id(&[&etc, &dbus]).as_deref(), Some(id), "etc invalid");
    std::fs::write(&etc, "fedcba9876543210fedcba9876543210\n").unwrap();
    assert_eq!(crate::linux::first_machine_id(&[&etc, &dbus]).as_deref(), Some("fedcba9876543210fedcba9876543210"), "etc first");
    // (#3277 re-review) The real list reads systemd's id first.
    assert_eq!(crate::linux::MACHINE_ID_PATHS, ["/etc/machine-id", "/var/lib/dbus/machine-id"]);
}

// ── host readings for the machine lens (observation only) ──

#[test]
fn per_cpu_ticks_come_from_proc_stat_in_cpu_order() {
    let stat = "cpu  10 1 5 100 2 1 1 0 0 0\ncpu0 4 1 2 50 1 0 0 0 0 0\ncpu1 6 0 3 50 1 1 1 0 0 0\nintr 123\n";
    let ticks = crate::linux::cpu_ticks_from_stat(stat).unwrap();
    assert_eq!(ticks.len(), 2, "the aggregate `cpu` line is not a core");
    // system folds in irq + softirq; idle folds in iowait.
    assert_eq!(
        ticks[0],
        CpuTicks {
            user: 4,
            nice: 1,
            system: 2,
            idle: 51
        }
    );
    assert_eq!(
        ticks[1],
        CpuTicks {
            user: 6,
            nice: 0,
            system: 5,
            idle: 51
        }
    );
    assert_eq!(crate::linux::cpu_ticks_from_stat("intr 1\n"), None);
}

/// Tiers, the shape macOS's `hw.perflevelN` reports, read from `cpufreq`
/// policies (`(online cpus, capacity, kHz)`).
fn tiers(policies: Vec<(Vec<usize>, Option<u64>, Option<u64>)>, online: usize) -> Vec<(String, usize, Option<u32>)> {
    let online: Vec<usize> = (0..online).collect();
    crate::linux::tiers_from_policies(policies, &online).into_iter().map(|(c, mhz)| (c.name, c.logical_cpus, mhz)).collect()
}

/// Each tier's tick positions, for `online` CPU numbers.
fn positions(policies: Vec<(Vec<usize>, Option<u64>, Option<u64>)>, online: &[usize]) -> Vec<(String, Vec<usize>)> {
    crate::linux::tiers_from_policies(policies, online).into_iter().map(|(c, _)| (c.name, c.tick_positions)).collect()
}

/// An M1 or M2: E cores first, then P cores, fastest tier first.
#[test]
fn asahi_tiers_are_fastest_first_with_their_mhz() {
    let t = tiers(vec![(vec![0, 1, 2, 3], Some(571), Some(600_000)), (vec![4, 5, 6, 7], Some(1024), Some(1_968_000))], 8);
    assert_eq!(t, vec![("Performance".into(), 4, Some(1968)), ("Efficiency".into(), 4, Some(600))]);
}

/// (#3279 review F2) An M1 Pro's two P-clusters are one Performance tier, so
/// each tier's busy percent and MHz come from its own CPUs.
#[test]
fn equal_capacity_clusters_are_one_tier() {
    let t = tiers(
        vec![
            (vec![0, 1], Some(512), Some(600_000)),
            (vec![2, 3, 4, 5], Some(1024), Some(3_000_000)),
            (vec![6, 7, 8, 9], Some(1024), Some(2_000_000)),
        ],
        10,
    );
    assert_eq!(t, vec![("Performance".into(), 8, Some(2500)), ("Efficiency".into(), 2, Some(600))]);
}

/// (#3279 review F3) x86's one policy per CPU, with no `cpu_capacity`, is
/// one "CPU" tier, not one cluster per CPU.
#[test]
fn one_policy_per_cpu_with_no_capacity_is_one_cpu_tier() {
    let t = tiers((0..4).map(|c| (vec![c], None, Some(2_000_000 + c as u64 * 1000))).collect(), 4);
    assert_eq!(t, vec![("CPU".into(), 4, Some(2001))]);
}

/// (#3279 re-review F1) An M1 Ultra numbers die 0's E and P cores, then
/// die 1's: each tier names its own CPUs' positions, not a block.
#[test]
fn a_tier_split_across_dies_names_its_own_cpus() {
    let p = |cpus: std::ops::Range<usize>, cap| (cpus.collect::<Vec<_>>(), Some(cap), None);
    let online: Vec<usize> = (0..20).collect();
    let t = positions(vec![p(0..2, 512), p(2..6, 1024), p(6..10, 1024), p(10..12, 512), p(12..16, 1024), p(16..20, 1024)], &online);
    let perf: Vec<usize> = (2..10).chain(12..20).collect();
    assert_eq!(t, vec![("Performance".into(), perf), ("Efficiency".into(), vec![0, 1, 10, 11])]);
}

/// An offline CPU has no `/proc/stat` line, so positions skip it: with cpu1
/// offline, cpu2 is at position 1.
#[test]
fn positions_follow_the_online_cpus() {
    let t = positions(vec![(vec![0, 2], Some(512), None), (vec![3, 4], Some(1024), None)], &[0, 2, 3, 4]);
    assert_eq!(t, vec![("Performance".into(), vec![2, 3]), ("Efficiency".into(), vec![0, 1])]);
    // A tier whose CPUs are all offline is dropped.
    let gone = positions(vec![(vec![0], Some(512), None), (vec![5], Some(1024), None)], &[0]);
    assert_eq!(gone, vec![("Efficiency".into(), vec![0])]);
}

/// `/proc/stat`'s CPU numbers; the aggregate `cpu` line is not one.
#[test]
fn online_cpus_come_from_proc_stat() {
    let stat = "cpu  10 1 5 100\ncpu0 4 1 2 50\ncpu2 6 0 3 50\nintr 1\n";
    assert_eq!(crate::linux::online_cpus_from_stat(stat), vec![0, 2]);
}

/// (#3279 review F1) No `cpufreq` (a VM, a container): one "CPU" tier of
/// every online CPU, its MHz unknown; nothing when no CPU was read.
#[test]
fn no_cpufreq_is_one_tier_of_the_online_cpus() {
    assert_eq!(tiers(vec![], 4), vec![("CPU".into(), 4, None)]);
    assert!(tiers(vec![], 0).is_empty());
}

#[test]
fn a_linux_memory_snapshot_comes_from_meminfo() {
    let meminfo = "MemTotal: 7715696 kB\nMemFree: 2171000 kB\nMemAvailable: 5071000 kB\nSwapTotal: 7714800 kB\nSwapFree: 6849000 kB\n";
    let m = crate::linux::memory_from_meminfo(meminfo).unwrap();
    assert_eq!(
        m.capacity_bytes,
        7715696 * 1024,
        "capacity is usable MemTotal, never rounded"
    );
    assert_eq!(m.free_bytes, Some(2171000 * 1024));
    assert_eq!(m.available_bytes, Some(5071000 * 1024));
    assert_eq!(m.swap_used_bytes, Some((7714800 - 6849000) * 1024));
    assert_eq!(
        m.used_pct(),
        Some(34),
        "(total - available) / total, rounded"
    );
    assert!(
        crate::linux::memory_from_meminfo("MemFree: 1 kB\n").is_none(),
        "no capacity, no snapshot"
    );
}

#[test]
fn this_linux_machine_reads_its_cpus_and_memory() {
    if std::env::consts::OS == "linux" {
        let ticks = LinuxPlatform.cpu_ticks().expect("/proc/stat");
        let clusters = LinuxPlatform.cpu_clusters();
        // The tiers cover every CPU exactly once.
        let mut positions: Vec<usize> = clusters.iter().flat_map(|c| c.tick_positions.clone()).collect();
        positions.sort_unstable();
        assert_eq!(positions, (0..ticks.len()).collect::<Vec<_>>());
        assert!(LinuxPlatform
            .memory_snapshot()
            .unwrap()
            .used_pct()
            .is_some());
    }
}

/// macOS reads these through darkmux-crew's mach probe until that moves
/// here; its strategy answers nothing, so nothing changes on a Mac.
#[test]
fn macos_and_other_leave_host_readings_to_their_existing_paths() {
    assert!(MacPlatform.cpu_clusters().is_empty() && MacPlatform.cpu_ticks().is_none());
    assert!(MacPlatform.memory_snapshot().is_none());
    assert!(OtherPlatform.cpu_ticks().is_none() && OtherPlatform.memory_snapshot().is_none());
}

/// A battery from `/sys/class/power_supply`: the Battery supply's charge,
/// status and current, and whether any Mains supply is online.
#[test]
fn a_linux_battery_comes_from_power_supply() {
    let full_on_ac = crate::linux::battery_from_supply(
        &[("type", "Battery"), ("present", "1"), ("capacity", "100"), ("status", "Full"), ("current_now", "0"), ("time_to_empty_now", "0")],
        true,
    )
    .unwrap();
    assert_eq!(
        full_on_ac,
        BatteryReading { charge_pct: 100, on_ac: true, charging: false, fully_charged: Some(true), current_ma: Some(0), minutes_to_empty: None }
    );
    let discharging = crate::linux::battery_from_supply(
        &[("type", "Battery"), ("capacity", "62"), ("status", "Discharging"), ("current_now", "-1250000"), ("time_to_empty_now", "5400")],
        false,
    )
    .unwrap();
    assert_eq!(discharging.current_ma, Some(-1250));
    assert_eq!(discharging.minutes_to_empty, Some(90));
    assert!(!discharging.on_ac && !discharging.charging && discharging.fully_charged == Some(false));
    let held = crate::linux::battery_from_supply(&[("type", "Battery"), ("capacity", "80"), ("status", "Not charging")], true).unwrap();
    assert!(held.on_ac && !held.charging && held.fully_charged == Some(false));
    assert!(crate::linux::battery_from_supply(&[("type", "Battery"), ("present", "0"), ("capacity", "50")], true).is_none(), "an absent pack is no battery");
    assert!(crate::linux::battery_from_supply(&[("type", "Battery"), ("status", "Full")], true).is_none(), "no charge, no reading");
}

#[test]
fn this_linux_machine_reads_its_battery_if_it_has_one() {
    if std::env::consts::OS == "linux" && std::path::Path::new("/sys/class/power_supply/macsmc-battery").exists() {
        let b = LinuxPlatform.battery().expect("the M2's battery");
        assert!(b.charge_pct <= 100);
    }
    assert!(MacPlatform.battery().is_none() && OtherPlatform.battery().is_none());
}

#[test]
fn this_linux_machine_reports_a_frequency_per_cluster() {
    if std::env::consts::OS == "linux" {
        assert_eq!(LinuxPlatform.cpu_cluster_mhz().len(), LinuxPlatform.cpu_clusters().len());
    }
    assert!(MacPlatform.cpu_cluster_mhz().is_empty() && OtherPlatform.cpu_cluster_mhz().is_empty());
}

// ── #3281: reclaimable memory, the doctor's RAM headroom figure ──────────

/// macOS: free plus inactive pages from `vm_stat`, times the page size. The
/// reader doctor ran before, moved.
#[test]
fn macos_reclaimable_is_free_plus_inactive_pages() {
    let vm_stat = "Mach Virtual Memory Statistics: (page size of 16384 bytes)\n\
                   Pages free:                              100000.\n\
                   Pages active:                            500000.\n\
                   Pages inactive:                           50000.\n";
    assert_eq!(macos::reclaimable_from_vm_stat(vm_stat, 16_384), Some(150_000 * 16_384));
    // A field vm_stat printed but that does not parse is no figure at all.
    assert_eq!(macos::reclaimable_from_vm_stat("Pages free:   .\n", 16_384), None);
}

#[test]
fn vm_stat_page_counts_ignore_separators() {
    assert_eq!(macos::parse_pages_field("                  1234567."), Some(1234567));
    assert_eq!(macos::parse_pages_field(" 1.234.567."), Some(1234567));
    assert_eq!(macos::parse_pages_field("        ."), None);
}

/// Linux: `MemAvailable`, the kernel's own estimate of what can be handed
/// out without swapping, reclaimable cache included.
#[test]
fn linux_reclaimable_is_memavailable() {
    let meminfo = "MemTotal:       16000000 kB\nMemFree:         1000000 kB\nMemAvailable:    9000000 kB\n";
    assert_eq!(linux::reclaimable_from_meminfo(meminfo), Some(9_000_000 * 1024));
    // A kernel too old to report MemAvailable gives no figure, not MemFree.
    assert_eq!(linux::reclaimable_from_meminfo("MemTotal: 16000000 kB\nMemFree: 1000000 kB\n"), None);
}

#[test]
fn an_os_with_no_strategy_reports_no_reclaimable_memory() {
    assert_eq!(OtherPlatform.reclaimable_memory_bytes(), None);
}

#[test]
fn a_fake_answers_the_reclaimable_memory_the_test_set() {
    let fake = FakePlatform { reclaimable_memory_bytes: Some(42), ..FakePlatform::default() };
    assert_eq!(fake.reclaimable_memory_bytes(), Some(42));
}

#[cfg(target_os = "linux")]
#[test]
fn this_linux_machine_reports_reclaimable_memory() {
    assert!(LinuxPlatform.reclaimable_memory_bytes().is_some_and(|b| b > 0));
}

/// (#3200) Field 22 of `/proc/<pid>/stat`, past a `comm` that holds spaces
/// and a `)`.
#[test]
fn linux_start_time_is_field_22_after_the_last_paren() {
    let stat = "4242 (tmux: server) a) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 200";
    assert_eq!(linux::start_ticks_from_proc_stat(stat), Some(987654));
    assert_eq!(linux::start_ticks_from_proc_stat("garbage"), None);
}

/// (#3200) The host's own strategy reads this process's start time, stably,
/// and a pid with no process reads as `None`.
#[cfg(unix)]
#[test]
fn the_current_strategy_reads_a_live_process_start_time_and_none_for_no_process() {
    let own = current().process_start_stamp(std::process::id());
    if current().id() == "other" {
        assert_eq!(own, None);
        return;
    }
    let own = own.expect("own start time is readable");
    assert!(own > 0);
    assert_eq!(current().process_start_stamp(std::process::id()), Some(own), "stable across reads");
    assert_eq!(current().process_start_stamp(u32::MAX >> 1), None, "no such process");
}

#[test]
fn the_fake_answers_only_the_stamps_the_test_set() {
    let fake = FakePlatform { process_start_stamps: vec![(7, 70)], ..Default::default() };
    assert_eq!(fake.process_start_stamp(7), Some(70));
    assert_eq!(fake.process_start_stamp(8), None);
}

/// (#3200) The pid-namespace link parses to its inode, and only that shape.
#[test]
fn linux_pid_namespace_is_the_ns_links_inode() {
    assert_eq!(linux::pid_namespace_from_link("pid:[4026531836]").as_deref(), Some("pid:4026531836"));
    assert_eq!(linux::pid_namespace_from_link("net:[4026531836]"), None);
    assert_eq!(linux::pid_namespace_from_link("pid:[]"), None);
    assert_eq!(MacPlatform.pid_namespace().as_deref(), Some("macos"));
    assert_eq!(OtherPlatform.pid_namespace(), None);
    let fake = FakePlatform { pid_namespace: Some("ns".into()), ..Default::default() };
    assert_eq!(fake.pid_namespace().as_deref(), Some("ns"));
}

// ── #3331: power and board sensors from hwmon ───────────────────────────

fn hwmon(chip: &str, sensor: &str, label: Option<&str>, value: i64) -> crate::linux::HwmonInput {
    crate::linux::HwmonInput { chip: chip.into(), sensor: sensor.into(), label: label.map(Into::into), value }
}

/// What darkbook's hwmon read on 2026-10-22 (13-inch M2 MacBook Pro, on
/// battery, idle), trimmed to one input of each kind.
fn darkbook_hwmon() -> Vec<crate::linux::HwmonInput> {
    vec![
        hwmon("macsmc_battery", "temp1", Some("temp"), 36_800),
        hwmon("macsmc_battery", "power1", None, -10_756_000),
        hwmon("macsmc_hwmon", "fan1", Some("Fan"), 1558),
        hwmon("macsmc_hwmon", "power1", Some("Total System Power"), 10_517_264),
        hwmon("macsmc_hwmon", "power4", Some("Heatpipe Power"), 5_001_028),
        hwmon("macsmc_hwmon", "temp1", Some("NAND Flash Temperature"), 52_580),
        hwmon("macsmc_hwmon", "temp2", Some("Battery Hotspot"), 36_799),
        hwmon("tas2764", "temp1", None, 31_000),
    ]
}

#[test]
fn linux_package_power_is_the_heatpipe_sensor_in_milliwatts() {
    assert_eq!(crate::linux::package_mw_from_hwmon(&darkbook_hwmon()), Some(5001.028));
}

#[test]
fn no_heatpipe_sensor_is_no_package_power_never_the_system_total() {
    let without: Vec<_> = darkbook_hwmon().into_iter().filter(|i| i.label.as_deref() != Some("Heatpipe Power")).collect();
    assert_eq!(crate::linux::package_mw_from_hwmon(&without), None);
    assert_eq!(crate::linux::package_mw_from_hwmon(&[hwmon("macsmc_hwmon", "power4", Some("Heatpipe Power"), -5)]), None);
}

#[test]
fn board_sensors_are_the_labeled_temperatures_and_the_fan() {
    let s = crate::linux::board_sensors_from_hwmon(&darkbook_hwmon()).unwrap();
    assert_eq!(
        s.temps_c.into_iter().collect::<Vec<_>>(),
        vec![("Battery Hotspot".to_string(), 36.8), ("NAND Flash Temperature".to_string(), 52.6)],
        "the battery's bare `temp` label and the unlabeled amp are skipped"
    );
    assert_eq!(s.fan_rpm, Some(1558));
}

#[test]
fn a_label_two_chips_share_is_keyed_by_chip() {
    let s = crate::linux::board_sensors_from_hwmon(&[hwmon("a", "temp1", Some("Die"), 40_000), hwmon("b", "temp1", Some("Die"), 41_000)]).unwrap();
    assert_eq!(s.temps_c.keys().cloned().collect::<Vec<_>>(), vec!["a Die".to_string(), "b Die".to_string()]);
    assert_eq!(s.fan_rpm, None, "fanless");
    assert!(crate::linux::board_sensors_from_hwmon(&[hwmon("tas2764", "temp1", None, 31_000)]).is_none());
}

#[test]
fn this_linux_machine_reads_its_smc_power_and_sensors_if_it_has_them() {
    if std::env::consts::OS == "linux" && std::fs::read_dir("/sys/class/hwmon").is_ok_and(|d| {
        d.flatten().any(|c| std::fs::read_to_string(c.path().join("name")).is_ok_and(|n| n.trim() == "macsmc_hwmon"))
    }) {
        let mw = LinuxPlatform.package_power_mw().expect("the SMC's heatpipe power");
        assert!(mw > 0.0 && mw < 200_000.0, "{mw} mW");
        assert!(!LinuxPlatform.board_sensors().expect("SMC sensors").temps_c.is_empty());
    }
    assert!(MacPlatform.package_power_mw().is_none() && MacPlatform.board_sensors().is_none());
    assert!(OtherPlatform.package_power_mw().is_none() && OtherPlatform.board_sensors().is_none());
}

#[test]
fn macos_install_time_is_the_newest_install_of_the_running_version() {
    let json = r#"{ "SPInstallHistoryDataType" : [
      { "_name" : "macOS 15.0", "install_date" : "2024-09-20T10:00:00Z", "package_source" : "package_source_apple" },
      { "_name" : "macOS 15.1", "install_date" : "2024-10-28T16:10:30Z", "package_source" : "package_source_apple" },
      { "_name" : "XProtectPlistConfigData", "install_date" : "2024-11-01T00:00:00Z" },
      { "_name" : "macOS 15.1", "install_date" : "2024-10-29T00:00:00Z" } ] }"#;
    assert_eq!(crate::macos::macos_installed_at(json, "15.1"), Some(1_730_160_000_000));
    assert_eq!(crate::macos::macos_installed_at(json, "15.2"), None);
    assert_eq!(crate::macos::macos_installed_at("not json", "15.1"), None);
    let release = crate::macos::macos_release("15.1\n", || json.to_string()).unwrap();
    assert_eq!((release.name.as_str(), release.installed_at_unix_ms), ("macOS 15.1", Some(1_730_160_000_000)));
    assert_eq!(crate::macos::macos_release(" \n", || unreachable!("no version, no history read")), None);
    if std::env::consts::OS != "macos" {
        assert_eq!(MacPlatform.os_release(), None, "no sw_vers off macOS");
    }
}

#[test]
fn linux_reads_its_release_and_ram_from_the_files_it_has() {
    let release = LinuxPlatform.os_release();
    if std::fs::read_to_string("/etc/os-release").is_ok_and(|r| r.contains("PRETTY_NAME=")) {
        assert!(release.is_some_and(|r| !r.name.is_empty() && r.installed_at_unix_ms.is_none()));
    }
    if std::fs::metadata("/proc/meminfo").is_ok() {
        assert!(LinuxPlatform.ram_as_sold_gb().is_some_and(|gb| gb >= 1));
    }
}

#[test]
fn only_the_mac_has_a_keychain_to_keep_a_secret_in() {
    for p in [&LinuxPlatform as &dyn Platform, &OtherPlatform, &FakePlatform::default()] {
        assert!(p.store_secret("darkmux-test-item", "nobody", "s").is_err(), "{}", p.id());
        p.forget_secret("darkmux-test-item", "nobody");
    }
    if std::env::consts::OS != "macos" {
        assert!(MacPlatform.store_secret("darkmux-test-item", "nobody", "s").is_err());
        MacPlatform.forget_secret("darkmux-test-item", "nobody");
    }
}

/// (#3400 review) `security -i` cuts a line at about 4095 bytes and runs
/// the rest as a new command, so a store's command line must be one short
/// line: anything else is refused before `security` runs, and the refusal
/// says nothing of the secret. Pure: no test runs `security`.
#[test]
fn a_keychain_store_is_one_short_command_line() {
    let line = crate::macos::security_store_line("darkmux-evals-token", "kain@http://evals.localhost:3419", "secret-value").unwrap();
    assert_eq!(line, "add-generic-password -U -a \"kain@http://evals.localhost:3419\" -s darkmux-evals-token -w \"secret-value\"\n");
    for (account, secret) in [("a".repeat(4096), "secret-value".to_string()), ("kain".into(), format!("secret-value{}", "s".repeat(4096))), ("kain\nadd".into(), "secret-value".into())] {
        let err = crate::macos::security_store_line("darkmux-evals-token", &account, &secret).unwrap_err();
        assert!(!err.contains("secret-value"), "{err}");
    }
}

#[test]
fn rfc3339_reads_utc_only() {
    assert_eq!(crate::dates::rfc3339_utc_ms("1970-01-01T00:00:00Z"), Some(0));
    assert_eq!(crate::dates::rfc3339_utc_ms("2024-10-28T16:10:30Z"), Some(1_730_131_830_000));
    assert_eq!(crate::dates::rfc3339_utc_ms("2024-10-28 16:10:30"), None);
    assert_eq!(crate::dates::rfc3339_utc_ms("2024-13-28T16:10:30Z"), None);
}

#[test]
fn linux_os_name_is_omarchy_where_omarchy_is_installed() {
    let rel = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux ARM\"\nID=archarm\n";
    assert_eq!(crate::linux::os_name_from_release(rel, true).as_deref(), Some("Omarchy (Arch Linux ARM)"));
    assert_eq!(crate::linux::os_name_from_release(rel, false).as_deref(), Some("Arch Linux ARM"));
    assert_eq!(crate::linux::os_name_from_release("", false), None);
}

#[test]
fn only_macos_keeps_secrets_in_the_keychain() {
    assert_eq!(MacPlatform.secret_store(), crate::SecretStore::Keychain);
    assert_eq!(LinuxPlatform.secret_store(), crate::SecretStore::PrivateFile);
    assert_eq!(OtherPlatform.secret_store(), crate::SecretStore::PrivateFile);
}

#[test]
fn an_asahi_machine_is_named_by_the_size_apple_sold() {
    assert_eq!(crate::linux::sold_size_on_apple(8), 8);
    assert_eq!(crate::linux::sold_size_on_apple(15), 16, "14.6 GiB of MemTotal on a 16 GB M1");
    assert_eq!(crate::linux::sold_size_on_apple(63), 64);
}

/// A Mac's buy-time config as `sysctl` and `ioreg` print it on an M5 Max:
/// level 0 is its six Super cores, level 1 its twelve Performance cores.
#[test]
fn macos_machine_config_reads_perf_levels_gpu_cores_and_model() {
    let head = "hw.model: Mac17,7\nhw.nperflevels: 2\n";
    assert_eq!(crate::macos::sysctl_value(head, "hw.model"), Some("Mac17,7"));
    assert_eq!(crate::macos::sysctl_value(head, "hw.nperflevels"), Some("2"));
    assert_eq!(crate::macos::sysctl_value(head, "hw.memsize"), None);
    let levels = "hw.perflevel0.name: Super\nhw.perflevel0.physicalcpu: 6\nhw.perflevel1.name: Performance\nhw.perflevel1.physicalcpu: 12\n";
    assert_eq!(crate::macos::perf_levels_from_sysctl(levels, 2), vec![("Super".to_string(), 6), ("Performance".to_string(), 12)]);
    // A level with no core count is not reported as zero, and with one level
    // unread the rest is not reported as the whole machine.
    assert_eq!(crate::macos::perf_levels_from_sysctl("hw.perflevel0.name: Super\n", 1), vec![]);
    assert_eq!(crate::macos::perf_levels_from_sysctl("hw.perflevel0.name: Super\nhw.perflevel0.physicalcpu: 6\n", 2), vec![]);
    let ioreg = "+-o AGXAcceleratorG17X  <class AGXAcceleratorG17X, id 0x1000003a5>\n    {\n      \"gpu-core-count\" = 40\n      \"IOClass\" = \"AGXAcceleratorG17X\"\n    }\n";
    assert_eq!(crate::macos::gpu_cores_from_ioreg(ioreg), Some(40));
    assert_eq!(crate::macos::gpu_cores_from_ioreg("\"gpu-core-count\" = 0\n"), None);
    assert_eq!(crate::macos::gpu_cores_from_ioreg(""), None);
}

#[test]
fn other_platforms_report_no_machine_config() {
    assert_eq!(crate::other::OtherPlatform.machine_config(), MachineConfig::default());
    let config = MachineConfig { gpu_cores: Some(10), ..Default::default() };
    assert_eq!(FakePlatform { machine_config: config.clone(), ..FakePlatform::default() }.machine_config(), config);
}

/// (#3448, moved from the residency ledger) macOS reads a process's
/// physical footprint (`proc_pid_rusage`'s `ri_phys_footprint`, a frozen
/// ABI): a plausible figure for this process, `None` for no process. A
/// wrong field offset would read as 0 or an absurd figure. Every other
/// strategy reads nothing, as the ledger did off macOS.
#[test]
fn the_current_strategy_reads_this_process_footprint_on_macos_and_nothing_elsewhere() {
    let own = current().process_footprint(std::process::id());
    if current().id() != "macos" {
        assert_eq!(own, None);
        return;
    }
    let fp = own.expect("own footprint is readable");
    assert!(fp > 1_000_000, "implausibly small: {fp} bytes; struct offset likely wrong");
    assert!(fp < 100 * (1u64 << 30), "implausibly large: {fp} bytes; struct offset likely wrong");
    assert_eq!(current().process_footprint(i32::MAX as u32), None, "no such process");
}

#[test]
fn the_fake_answers_only_the_footprints_the_test_set() {
    let fake = FakePlatform { process_footprints: vec![(7, 7_000)], ..Default::default() };
    assert_eq!(fake.process_footprint(7), Some(7_000));
    assert_eq!(fake.process_footprint(8), None);
}

/// (#3453) Where an XDG-layout tool keeps per-user data: `$XDG_DATA_HOME`
/// when it is an absolute path, else `~/.local/share`; a relative or empty
/// `XDG_DATA_HOME` is ignored, as the base-directory spec says.
#[test]
fn xdg_data_home_is_the_env_when_absolute_else_local_share() {
    use std::ffi::OsString;
    use std::path::PathBuf;
    let home = Some(OsString::from("/Users/me"));
    assert_eq!(xdg_data_home_from(None, home.clone()), Some(PathBuf::from("/Users/me/.local/share")));
    assert_eq!(xdg_data_home_from(Some("/x/data".into()), home.clone()), Some(PathBuf::from("/x/data")));
    assert_eq!(xdg_data_home_from(Some("rel/data".into()), home.clone()), Some(PathBuf::from("/Users/me/.local/share")));
    assert_eq!(xdg_data_home_from(Some("".into()), home), Some(PathBuf::from("/Users/me/.local/share")));
    assert_eq!(xdg_data_home_from(None, None), None);
    assert_eq!(xdg_data_home_from(None, Some("".into())), None);
}

/// (#3453) macOS and Linux both answer the XDG location from this process's
/// environment (rec keeps its install there on both); an OS with no strategy
/// answers none; the fake answers what the test set.
#[test]
fn each_strategy_answers_the_xdg_data_home() {
    let env = xdg_data_home_from(std::env::var_os("XDG_DATA_HOME"), std::env::var_os("HOME"));
    assert!(env.is_some(), "a test host has a home");
    assert_eq!(MacPlatform.xdg_data_home(), env);
    assert_eq!(LinuxPlatform.xdg_data_home(), env);
    assert_eq!(OtherPlatform.xdg_data_home(), None);
    let fake = FakePlatform { xdg_data_home: Some("/fake/share".into()), ..Default::default() };
    assert_eq!(fake.xdg_data_home(), Some(std::path::PathBuf::from("/fake/share")));
    assert_eq!(FakePlatform::default().xdg_data_home(), None);
}

/// (#3453 review) `lsof -Fp`'s answer is one `p<pid>` line per process
/// (other field lines ignored); nothing listening is an empty list.
#[test]
fn lsof_pid_lines_parse_to_the_listening_pids() {
    assert_eq!(crate::pids_from_lsof_fp("p42854\nf3\np42855\nf7\n"), vec![42854, 42855]);
    assert_eq!(crate::pids_from_lsof_fp(""), Vec::<u32>::new());
    assert_eq!(crate::pids_from_lsof_fp("pnope\nf3\n"), Vec::<u32>::new());
}

/// (#3453 review) `/proc/net/tcp{,6}`: the inodes of the sockets in LISTEN
/// (`0A`) on `port`, whatever address they bound; another port or state is
/// not one. A socket fd's link names its inode.
#[test]
fn proc_net_tcp_listeners_on_a_port_are_read_by_inode() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41231 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41232 1 0000000000000000 100 0 0 10 0\n\
   2: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41233 1 0000000000000000 100 0 0 10 0\n\
   3: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n";
    assert_eq!(crate::linux::listening_inodes(table, 8080), vec![41231, 41232]);
    assert_eq!(crate::linux::listening_inodes(table, 9), Vec::<u64>::new());
    assert_eq!(crate::linux::socket_inode("socket:[41231]"), Some(41231));
    assert_eq!(crate::linux::socket_inode("pipe:[41231]"), None);
}

/// (#3453 review) Who listens on a loopback port, and a process's group,
/// asked of this host's strategy: this test's own listener is this process,
/// in this process's group; an unbound port has no listener. An OS with no
/// strategy cannot say; the fake answers what the test set.
#[test]
fn the_host_names_a_ports_listener_and_a_process_group() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let me = std::process::id();
    let host = current();
    assert_eq!(host.tcp_listeners(port), Some(vec![me]));
    drop(listener);
    assert_eq!(host.tcp_listeners(port), Some(vec![]));
    let own_group = unsafe { libc::getpgid(0) } as u32;
    assert_eq!(host.process_group(me), Some(own_group));
    assert_eq!(host.process_group(u32::MAX >> 1), None, "no such process");
    assert_eq!((OtherPlatform.tcp_listeners(port), OtherPlatform.process_group(me)), (None, None));
    let fake = FakePlatform { tcp_listeners: vec![(port, vec![7])], process_groups: vec![(7, 5)], ..Default::default() };
    assert_eq!((fake.tcp_listeners(port), fake.tcp_listeners(1), fake.process_group(7)), (Some(vec![7]), Some(vec![]), Some(5)));
}

/// `lsof -Fpn`: every process holding a peer's end of a connection to the
/// listener, from the name lines that run from a peer to the listener; the
/// listener's own ends (the reverse direction) and other ports are not.
#[test]
fn lsof_names_the_process_at_each_peers_end() {
    let local: std::net::SocketAddr = "127.0.0.1:8767".parse().unwrap();
    let out = "p11936\nf4\nn127.0.0.1:8767->127.0.0.1:55807\nf5\nn127.0.0.1:8767->127.0.0.1:55808\n\
               p92120\nf390\nn127.0.0.1:55807->127.0.0.1:8767\nf391\nn127.0.0.1:55808->127.0.0.1:8767\n\
               p500\nf3\nn127.0.0.1:55809->127.0.0.1:9999\n";
    assert_eq!(
        crate::peers_from_lsof_fpn(out, local),
        vec![("127.0.0.1:55807".parse().unwrap(), 92120), ("127.0.0.1:55808".parse().unwrap(), 92120)]
    );
    assert!(crate::peers_from_lsof_fpn("p11936\nf4\nn127.0.0.1:8767->127.0.0.1:55807\n", local).is_empty());
    assert!(crate::peers_from_lsof_fpn("", local).is_empty());
}

/// `/proc/net/tcp{,6}`: every ESTABLISHED socket whose other end is the
/// listener, by its own end; the listener's own sockets (the reverse), a
/// LISTEN socket and another port are not. A dual-stack client's socket in
/// `tcp6` shows its v4-mapped address.
#[test]
fn proc_net_tcp_names_the_socket_at_each_peers_end() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:223F 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41231 1 0000000000000000 100 0 0 10 0\n\
   1: 0100007F:223F 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 41232 1 0000000000000000 100 0 0 10 0\n\
   2: 0100007F:D431 0100007F:223F 01 00000000:00000000 00:00000000 00000000  1000        0 41233 1 0000000000000000 100 0 0 10 0\n\
   3: 0100007F:D432 0100007F:0001 01 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n";
    let local: std::net::SocketAddr = "127.0.0.1:8767".parse().unwrap();
    let peer: std::net::SocketAddr = "127.0.0.1:54321".parse().unwrap();
    assert_eq!(crate::linux::connected_to(table, local), vec![(peer, 41233)]);
    assert!(crate::linux::connected_to(table, "127.0.0.1:2".parse().unwrap()).is_empty());
    let table6 = "  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0000000000000000FFFF00000100007F:D431 0000000000000000FFFF00000100007F:223F 01 00000000:00000000 00:00000000 00000000  1000        0 51233 1 0000000000000000 20 4 30 10 -1\n";
    assert_eq!(crate::linux::connected_to(table6, local), vec![(peer, 51233)]);
}

/// A `/proc/net/tcp{,6}` address reads as IPv4 from 8 hex digits, as IPv6
/// from 32 (a v4-mapped one as its IPv4 address); anything else, and a row
/// that is not ESTABLISHED or does not read whole, is no connection.
#[test]
fn proc_net_addresses_and_rows_read_whole_or_not_at_all() {
    use crate::linux::{established, proc_net_addr};
    assert_eq!(proc_net_addr("0100007F:1F90"), Some("127.0.0.1:8080".parse().unwrap()));
    assert_eq!(proc_net_addr("0000000000000000FFFF00000100007F:1F90"), Some("127.0.0.1:8080".parse().unwrap()));
    assert_eq!(proc_net_addr("00000000000000000000000001000000:1F90"), Some("[::1]:8080".parse().unwrap()));
    for bad in ["0100007F", "0100007F:ZZZZ", "0100:1F90", "0100007G:1F90", "0000000000000000FFFF0000:1F90"] {
        assert_eq!(proc_net_addr(bad), None, "{bad}");
    }
    let row = |st: &str, inode: &str| format!("   1: 0100007F:D431 0100007F:223F {st} 00000000:00000000 00:00000000 00000000  1000        0 {inode} 1");
    let (own, other) = ("127.0.0.1:54321".parse().unwrap(), "127.0.0.1:8767".parse().unwrap());
    assert_eq!(established(&row("01", "41233")), Some((own, other, 41233)));
    assert_eq!(established(&row("0A", "41233")), None, "not ESTABLISHED");
    assert_eq!(established(&row("01", "x")), None, "no inode");
    assert_eq!(established("   1: nonsense 0100007F:223F 01"), None, "an end that does not read");
    assert_eq!(established("   1: 0100007F:D431"), None, "too short");
}

/// This host's strategy names the process at the other end of every
/// loopback connection to a listener at once: here, this test's own process
/// for both of two connections. An OS with no strategy cannot say; the fake
/// answers what the test set.
#[test]
fn the_host_names_the_process_at_each_loopback_connections_other_end() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let local = listener.local_addr().unwrap();
    let clients = [std::net::TcpStream::connect(local).unwrap(), std::net::TcpStream::connect(local).unwrap()];
    let _accepted = [listener.accept().unwrap(), listener.accept().unwrap()];
    let mut found = current().tcp_peers_of(local).unwrap();
    found.sort();
    let mut want: Vec<_> = clients.iter().map(|c| (c.local_addr().unwrap(), std::process::id())).collect();
    want.sort();
    assert_eq!(found, want);
    assert_eq!(OtherPlatform.tcp_peers_of(local), None);
    let fake = FakePlatform { tcp_peers: vec![(want[0].0, 7)], ..Default::default() };
    assert_eq!(fake.tcp_peers_of(local), Some(vec![(want[0].0, 7)]));
}

// ── memory pressure: the kernel's level, swap, and the compressor ────────

#[test]
fn macos_pressure_level_is_the_kernels_one_two_four() {
    use crate::PressureLevel::{Critical, Normal, Warning};
    assert_eq!(macos::pressure_level_from(1), Some(Normal));
    assert_eq!(macos::pressure_level_from(2), Some(Warning));
    assert_eq!(macos::pressure_level_from(4), Some(Critical));
    // A value the kernel does not define is no reading, never "normal".
    assert_eq!(macos::pressure_level_from(0), None);
    assert_eq!(macos::pressure_level_from(3), None);
    assert!(Normal < Warning && Warning < Critical);
}

#[test]
fn macos_sysctl_integers_are_four_or_eight_bytes() {
    assert_eq!(macos::uint_from_sysctl(&4u32.to_ne_bytes()), Some(4));
    assert_eq!(macos::uint_from_sysctl(&40_265_312u64.to_ne_bytes()), Some(40_265_312));
    assert_eq!(macos::uint_from_sysctl(&[0u8; 3]), None);
}

#[test]
fn macos_swap_is_read_from_xsw_usage() {
    let (total, avail, used) = (48u64 << 30, 2u64 << 30, 46u64 << 30);
    let mut raw: Vec<u8> = [total, avail, used].iter().flat_map(|w| w.to_ne_bytes()).collect();
    raw.extend([0u8; 8]);
    assert_eq!(macos::swap_from_xsw_usage(&raw), Some((used, avail)));
    assert_eq!(macos::swap_from_xsw_usage(&raw[..20]), None, "a short struct is no reading");
}

#[test]
fn the_compressor_figure_is_the_fuller_of_its_limits() {
    // Segments at 97% and compressed pages at 60%: 97.
    let pairs = [(Some(2_450_000), Some(2_516_582)), (Some(24_159_187), Some(40_265_312))];
    assert_eq!(macos::fuller_pct(&pairs), Some(97));
    assert_eq!(macos::fuller_pct(&[(Some(5), Some(0)), (None, Some(10))]), None, "no pair with both figures");
    assert_eq!(macos::fuller_pct(&[(Some(30), Some(10))]), Some(100), "capped");
}

/// The raw sysctl values of the 11-10 panic: the kernel's level still
/// normal, the compressor at 97% of its segments limit and 60% of its
/// compressed-pages limit, 24 GiB of swap in use of 45 GiB allocated.
fn panic_sysctls() -> macos::PressureSysctls {
    let swap: Vec<u8> = [45u64 << 30, 21u64 << 30, 24u64 << 30, 0].iter().flat_map(|w| w.to_ne_bytes()).collect();
    macos::PressureSysctls {
        level: Some(1),
        swapusage: Some(swap),
        segments: (Some(2_450_000), Some(2_516_582)),
        pages: (Some(24_159_187), Some(40_265_312)),
    }
}

#[test]
fn a_macs_raw_sysctls_become_its_pressure_reading() {
    let p = macos::pressure_from_sysctls(&panic_sysctls()).expect("a reading");
    assert_eq!(p.level, Some(crate::PressureLevel::Normal));
    assert_eq!(p.compressor_segments_pct, Some(97), "the panic's segments figure, on its own");
    assert_eq!(p.compressor_pct, Some(97), "the fuller of segments (97%) and pages (60%)");
    assert_eq!((p.swap_used_bytes, p.swap_free_bytes), (Some(24 << 30), Some(21 << 30)));
    // Pages fuller than segments: the fuller figure follows the pages, the
    // segments figure stays the segments'.
    let pages = macos::PressureSysctls { segments: (Some(10), Some(100)), pages: (Some(95), Some(100)), ..panic_sysctls() };
    let p = macos::pressure_from_sysctls(&pages).unwrap();
    assert_eq!((p.compressor_pct, p.compressor_segments_pct), (Some(95), Some(10)));
}

#[test]
fn a_macs_missing_or_unknown_sysctls_are_no_figure_never_a_stand_in() {
    let none = macos::PressureSysctls { level: None, swapusage: None, segments: (None, None), pages: (None, None) };
    assert_eq!(macos::pressure_from_sysctls(&none), None, "nothing read is no reading");
    // A level the kernel does not define, a zero limit and a short swap
    // struct each leave their figure out; what was read stays.
    let partial = macos::PressureSysctls {
        level: Some(3),
        swapusage: Some(vec![0u8; 20]),
        segments: (Some(5), Some(0)),
        pages: (Some(50), Some(100)),
    };
    let p = macos::pressure_from_sysctls(&partial).expect("the pages figure was read");
    assert_eq!((p.level, p.swap_used_bytes, p.swap_free_bytes), (None, None, None));
    assert_eq!((p.compressor_pct, p.compressor_segments_pct), (Some(50), None));
    let swap_only = macos::PressureSysctls { swapusage: panic_sysctls().swapusage, ..none };
    assert_eq!(macos::pressure_from_sysctls(&swap_only).and_then(|p| p.swap_used_bytes), Some(24 << 30));
    // Saturating: a count past its limit is capped at 100%, never wraps.
    let over = macos::PressureSysctls { level: None, swapusage: None, segments: (Some(u64::MAX), Some(3)), pages: (None, None) };
    assert_eq!(macos::pressure_from_sysctls(&over).and_then(|p| p.compressor_segments_pct), Some(100));
}

#[test]
fn linux_pressure_is_its_swap_and_no_level() {
    let meminfo = "MemTotal: 16000000 kB\nSwapTotal:  8000000 kB\nSwapFree:   6000000 kB\n";
    let p = linux::pressure_from_meminfo(meminfo).unwrap();
    assert_eq!((p.swap_used_bytes, p.swap_free_bytes), (Some(2_000_000 * 1024), Some(6_000_000 * 1024)));
    assert_eq!((p.level, p.compressor_pct, p.compressor_segments_pct), (None, None, None));
    assert_eq!(linux::pressure_from_meminfo("MemTotal: 16000000 kB\n"), None);
}

#[test]
fn an_os_with_no_strategy_reads_no_pressure_and_a_fake_answers_what_was_set() {
    assert_eq!(OtherPlatform.memory_pressure(), None);
    let set = crate::MemoryPressure { level: Some(crate::PressureLevel::Critical), ..Default::default() };
    let fake = FakePlatform { memory_pressure: Some(set), ..FakePlatform::default() };
    assert_eq!(fake.memory_pressure(), Some(set));
}

/// On a Mac the live reading names a level the kernel defines.
#[cfg(target_os = "macos")]
#[test]
fn a_mac_reads_its_own_pressure_level() {
    let p = MacPlatform.memory_pressure().expect("a Mac reads its pressure");
    assert!(p.level.is_some(), "{p:?}");
    // The segments figure the gate trips early on is read on its own, and
    // the compressor's fuller figure is never under it.
    let segments = p.compressor_segments_pct.expect("the compressor's segments against their limit");
    assert!(p.compressor_pct.is_some_and(|fuller| fuller >= segments), "{p:?}");
}

// ─── (#3544) Who signed a running app, and the Allow dialog ─────────────

/// `codesign -d --verbose=2 <pid>` for the running Mux Bot, as captured on
/// this project's Mac (2026-11-11).
const MUX_BOT_SHOWN: &str = "Executable=/Applications/Mux Bot.app/Contents/MacOS/Mux Bot
Identifier=com.darklyenergized.darkmuxapp
Format=app bundle with Mach-O thin (arm64)
CodeDirectory v=20500 size=464 flags=0x10000(runtime) hashes=3+7 location=embedded
Signature size=4790
Authority=Apple Development: Test Developer (ABCDE12345)
Authority=Apple Worldwide Developer Relations Certification Authority
Authority=Apple Root CA
Signed Time=Nov 10, 2026 at 2:53:57 PM
TeamIdentifier=5LDXYW8B6T
Runtime Version=26.2.0
";

/// The executable the kernel names for the Mux Bot process above.
const MUX_EXE: &str = "/Applications/Mux Bot.app/Contents/MacOS/Mux Bot";

/// A `codesign -d` body (the lines after `Executable=`) as it describes
/// [`MUX_EXE`].
fn named(body: &str) -> String {
    format!("Executable={MUX_EXE}\n{body}")
}

/// [`macos::signed_app_from`] for a process the kernel says runs [`MUX_EXE`].
fn from_codesign(
    display: Result<String, String>,
    verify: impl FnOnce(&str) -> Result<(), String>,
    icon_of: impl FnOnce(&std::path::Path, &str) -> Option<std::path::PathBuf>,
) -> Result<SignedApp, String> {
    macos::signed_app_from(Ok(MUX_EXE.into()), display, verify, icon_of)
}

/// An icon finder for a test that is not about icons.
fn no_icon(_exe: &std::path::Path, _requirement: &str) -> Option<std::path::PathBuf> {
    None
}

/// A verifier standing in for `codesign --verify -R`, with codesign's own
/// semantics for a signature whose leaf certificate does NOT chain to
/// Apple but whose fields read like the team's (a self-signed certificate
/// with the team's OU): only a requirement with Apple's anchor refuses it.
fn self_signed_verify(requirement: &str) -> Result<(), String> {
    match requirement.contains("anchor apple generic") {
        true => Err("test-requirement: code failed to satisfy specified code requirement(s)".into()),
        false => Ok(()),
    }
}

#[test]
fn a_signed_hardened_app_is_named_by_its_team_bundle_and_signer() {
    let mut asked = String::new();
    let app = from_codesign(Ok(MUX_BOT_SHOWN.into()), |r| {
        asked = r.to_string();
        Ok(())
    }, no_icon)
    .unwrap();
    assert_eq!(
        app,
        SignedApp {
            team: "5LDXYW8B6T".into(),
            bundle: "com.darklyenergized.darkmuxapp".into(),
            signer: "Apple Development: Test Developer (ABCDE12345)".into(),
            icon: None,
        }
    );
    assert_eq!(
        asked,
        "anchor apple generic and certificate leaf[subject.OU] = \"5LDXYW8B6T\" and identifier \"com.darklyenergized.darkmuxapp\""
    );
}

/// A signature that does not chain to Apple never names an app, however its
/// team and identifier read: the requirement carries Apple's anchor.
#[test]
fn a_signature_not_chained_to_apple_is_not_named() {
    let e = from_codesign(Ok(MUX_BOT_SHOWN.into()), self_signed_verify, no_icon).unwrap_err();
    assert!(e.contains("does not hold"), "{e}");
}

/// `codesign -s -` (ad hoc) as `codesign -d` reports it: no team. Even
/// with a team line forged in and a verifier that passes, the ad hoc flag refuses it.
#[test]
fn an_ad_hoc_signature_is_never_named() {
    let ad_hoc = "Identifier=org.python.python\nCodeDirectory v=20400 size=298 flags=0x2(adhoc) hashes=3+3 location=embedded\nSignature=adhoc\nTeamIdentifier=not set\n";
    let e = from_codesign(Ok(named(ad_hoc)), |_| Ok(()), no_icon).unwrap_err();
    assert!(e.contains("no developer team"), "{e}");
    let forged = "Identifier=x.y\nCodeDirectory v=20400 size=298 flags=0x10002(adhoc,runtime) hashes=3+3 location=embedded\nAuthority=Me\nTeamIdentifier=5LDXYW8B6T\n";
    let e = from_codesign(Ok(named(forged)), |_| Ok(()), no_icon).unwrap_err();
    assert!(e.contains("ad hoc"), "{e}");
}

#[test]
fn an_apple_system_binary_has_no_team_and_is_not_named() {
    let ls = "Identifier=com.apple.ls\nCodeDirectory v=20400 size=325 flags=0x0(none) hashes=5+2 location=embedded\nAuthority=Software Signing\nTeamIdentifier=not set\n";
    assert!(from_codesign(Ok(named(ls)), |_| Ok(()), no_icon).is_err());
}

#[test]
fn an_app_without_the_hardened_runtime_is_not_named() {
    let shown = MUX_BOT_SHOWN.replace("flags=0x10000(runtime)", "flags=0x0(none)");
    let e = from_codesign(Ok(shown), |_| Ok(()), no_icon).unwrap_err();
    assert!(e.contains("hardened runtime"), "{e}");
}

/// Docker Desktop's processes (team `9BNSXJN65R`, as `codesign -d` names
/// `com.docker.backend` here) carry every container's connection, so they
/// are never named as an app, however valid the signature.
#[test]
fn docker_desktops_forwarder_is_never_named() {
    let docker = "Identifier=com.docker.docker\nCodeDirectory v=20500 size=1527965 flags=0x10000(runtime) hashes=47738+7 location=embedded\nAuthority=Developer ID Application: Docker Inc (9BNSXJN65R)\nTeamIdentifier=9BNSXJN65R\n";
    let e = from_codesign(Ok(named(docker)), |_| Ok(()), no_icon).unwrap_err();
    assert!(e.contains("container traffic"), "{e}");
}

/// A team or identifier that could change the requirement's meaning is
/// refused before a requirement is built from it.
#[test]
fn an_identifier_that_could_rewrite_the_requirement_is_refused() {
    let shown = MUX_BOT_SHOWN.replace(
        "Identifier=com.darklyenergized.darkmuxapp",
        "Identifier=x\" or identifier \"y",
    );
    let mut built = false;
    assert!(from_codesign(Ok(shown), |_| {
        built = true;
        Ok(())
    }, no_icon)
    .is_err());
    assert!(!built, "no requirement was verified");
    assert!(!macos::is_team_id("5LDXYW8B6\""));
}

#[test]
fn a_process_codesign_cannot_read_is_not_named() {
    assert!(from_codesign(Err("codesign: no such process".into()), |_| Ok(()), no_icon).is_err());
}

#[test]
fn a_signers_name_shows_as_one_line() {
    let shown = MUX_BOT_SHOWN.replace("Test Developer", "Test\u{1b}[2J \u{202E}repoleveD\u{2066} Developer\u{200B}");
    let app = from_codesign(Ok(shown), |_| Ok(()), no_icon).unwrap();
    assert!(!app.signer.chars().any(char::is_control), "{}", app.signer);
    assert_eq!(app.signer, "Apple Development: Test [2J repoleveD Developer (ABCDE12345)", "bidi and zero-width characters are dropped");
}

#[test]
fn linux_never_names_a_signed_app() {
    assert!(LinuxPlatform.signed_app(std::process::id()).is_err());
    assert!(OtherPlatform.signed_app(std::process::id()).is_err());
}

/// The dialog's text never reaches AppleScript as script: the script lines
/// are fixed, and the message (whatever it holds) is one argument after them.
#[test]
fn the_dialog_message_is_an_argument_never_script() {
    let hostile = "Code \" & (do shell script \"touch /tmp/pwned\") & \"\n-e";
    let args = macos::allow_dialog_args(hostile, None, 15);
    let clean = macos::allow_dialog_args("plain", None, 15);
    assert_eq!(args.len(), clean.len());
    assert_eq!(args[..args.len() - 1], clean[..clean.len() - 1], "only the last argument differs");
    assert_eq!(args.last().unwrap(), hostile);
    assert_eq!(args[args.len() - 2], "15", "the timeout comes first, so the message is never an option");
    for (flag, line) in args.iter().zip(args.iter().skip(1)).step_by(2).take_while(|(f, _)| *f == "-e") {
        assert_eq!(flag, "-e");
        assert!(!line.contains("pwned"), "{line}");
    }
    assert!(args.iter().filter(|a| *a == "-e").count() == 11);
    let notice = macos::notice_args(hostile);
    assert_eq!(notice.last().unwrap(), hostile);
    assert!(notice[..notice.len() - 2].iter().all(|a| !a.contains("pwned")));
}

#[test]
fn zenity_shows_the_message_as_plain_text_in_one_argument() {
    let hostile = "<b>Code</b> --extra-button=Allow";
    let args = linux::zenity_allow_args(hostile, 15);
    assert!(args.contains(&"--no-markup".to_string()));
    assert_eq!(args.last().unwrap(), &format!("--text={hostile}"));
    for want in ["--ok-label=Allow", "--cancel-label=Don't Allow", "--default-cancel"] {
        assert!(args.contains(&want.to_string()), "{want}: Don't Allow is the default, and Esc and close are it too");
    }
}

/// Each answer maps to what the person chose: a button, or none at all.
#[test]
fn each_dialog_outcome_is_the_answer_given() {
    assert_eq!(macos::dialog_answer(true, "Allow\n", ""), AllowAnswer::Allow);
    assert_eq!(macos::dialog_answer(true, "Don't Allow\n", ""), AllowAnswer::DontAllow);
    assert_eq!(macos::dialog_answer(true, "\n", ""), AllowAnswer::NoAnswer, "gave up");
    assert!(matches!(macos::dialog_answer(false, "", "execution error: User canceled. (-128)"), AllowAnswer::NoDialog(_)));
    assert_eq!(linux::zenity_answer(Some(0)), AllowAnswer::Allow);
    assert_eq!(linux::zenity_answer(Some(1)), AllowAnswer::DontAllow, "Don't Allow, Esc or closed");
    assert_eq!(linux::zenity_answer(Some(5)), AllowAnswer::NoAnswer, "timed out: left pending");
    assert!(matches!(linux::zenity_answer(Some(255)), AllowAnswer::NoDialog(_)));
}

#[test]
fn a_dialog_tool_is_found_only_as_an_executable_on_path() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let tool = dir.path().join("zenity");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    assert_eq!(linux::on_path("zenity", dir.path().as_os_str()), None, "not executable");
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(linux::on_path("zenity", dir.path().as_os_str()), Some(tool));
}

/// (#3544) The real macOS check against a pid you pass in:
/// `DARKMUX_SIGNED_APP_PID=<pid> cargo nextest run -p darkmux-platform
/// signed_app_live --run-ignored only --no-capture`.
#[test]
#[ignore = "reads a real process's signature; pass DARKMUX_SIGNED_APP_PID"]
fn signed_app_live() {
    let pid: u32 = std::env::var("DARKMUX_SIGNED_APP_PID").expect("DARKMUX_SIGNED_APP_PID").parse().unwrap();
    match current().signed_app(pid) {
        Ok(app) => println!("pid {pid}: VERIFIED as {app}, signed by {}, icon {:?}", app.signer, app.icon),
        Err(why) => println!("pid {pid}: NOT VERIFIED: {why}"),
    }
}

/// (#3544) Shows the real Allow dialog once, with a sample code:
/// `DARKMUX_SHOW_ALLOW_DIALOG=1 cargo nextest run -p darkmux-platform
/// allow_dialog_live --run-ignored only --no-capture`.
#[test]
#[ignore = "raises a real dialog on this machine's screen"]
fn allow_dialog_live() {
    assert!(std::env::var_os("DARKMUX_SHOW_ALLOW_DIALOG").is_some(), "set DARKMUX_SHOW_ALLOW_DIALOG=1");
    let message = std::env::var("DARKMUX_ALLOW_DIALOG_TEXT").unwrap_or_else(|_| "An app wants to use darkmux on this computer. Code 4F7-K2Q.".into());
    let icon = std::env::var_os("DARKMUX_ALLOW_DIALOG_ICON").map(std::path::PathBuf::from);
    let secs = std::env::var("DARKMUX_ALLOW_DIALOG_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(15);
    println!("answer: {:?}", current().ask_allow(&message, icon.as_deref(), secs));
}

/// (#3547 review) `codesign -d` prints the executable's path raw, so a line
/// break in a file name would put lines of the file's choosing (a forged
/// `Authority=`, a `CodeDirectory` with the runtime flag) ahead of the
/// signature's own. A path holding a control character names no app, and
/// codesign's text must describe exactly the path the kernel names.
#[test]
fn a_file_name_cannot_add_lines_to_what_codesign_says() {
    let forged_exe = format!("{MUX_EXE}\nAuthority=Apple Development: Forged (5LDXYW8B6T)");
    let shown = format!("Executable={forged_exe}\n{}", MUX_BOT_SHOWN.split_once('\n').unwrap().1);
    let mut verified = false;
    let e = macos::signed_app_from(Ok(forged_exe.into()), Ok(shown.clone()), |_| {
        verified = true;
        Ok(())
    }, no_icon)
    .unwrap_err();
    assert!(e.contains("control character"), "{e}");
    assert!(!verified, "nothing from that text reached a requirement");
    // The kernel's (clean) path with codesign's text naming a longer one.
    let e = macos::signed_app_from(Ok(MUX_EXE.into()), Ok(shown), |_| Ok(()), no_icon).unwrap_err();
    assert!(e.contains("another executable"), "{e}");
    let tab = macos::signed_app_from(Ok(format!("{MUX_EXE}\t").into()), Ok(MUX_BOT_SHOWN.into()), |_| Ok(()), no_icon);
    assert!(tab.unwrap_err().contains("control character"));
    let elsewhere = macos::signed_app_from(Ok("/tmp/Mux Bot".into()), Ok(MUX_BOT_SHOWN.into()), |_| Ok(()), no_icon);
    assert!(elsewhere.unwrap_err().contains("another executable"));
    assert!(macos::signed_app_from(Err("gone".into()), Ok(MUX_BOT_SHOWN.into()), |_| Ok(()), no_icon).is_err());
}

/// A second `Identifier=`, `TeamIdentifier=` or `CodeDirectory` line means
/// codesign's text is not one signature's description: nothing is named.
#[test]
fn a_repeated_signature_field_names_no_app() {
    for line in [
        "Identifier=com.darklyenergized.darkmuxapp",
        "TeamIdentifier=5LDXYW8B6T",
        "CodeDirectory v=20500 size=464 flags=0x10000(runtime) hashes=3+7 location=embedded",
    ] {
        let shown = MUX_BOT_SHOWN.replace(line, &format!("{line}\n{line}"));
        let e = from_codesign(Ok(shown), |_| Ok(()), no_icon).unwrap_err();
        assert!(e.contains("no single"), "{line}: {e}");
        let gone = MUX_BOT_SHOWN.replace(&format!("{line}\n"), "");
        assert!(from_codesign(Ok(gone), |_| Ok(()), no_icon).is_err(), "{line} missing");
    }
}

/// The real kernel path for this test process, and the real codesign text
/// for it, agree (so the binding above does not refuse real processes).
#[cfg(target_os = "macos")]
#[test]
fn the_kernels_path_is_the_one_codesign_names() {
    let pid = std::process::id();
    let exe = macos::process_path(pid).unwrap();
    let shown = macos::codesign_display(pid).unwrap();
    assert!(macos::codesign_fields(&exe, &shown).is_ok(), "{exe:?}\n{shown}");
    assert!(macos::process_path(u32::MAX).is_err());
}

/// A verified app is shown with its bundle's own icon, looked up from the
/// executable `codesign` names; an unverified one is never looked up, so
/// its icon (which an impostor chose) is never shown.
#[test]
fn only_a_verified_app_gets_its_own_icon() {
    let mut looked = None;
    let app = from_codesign(Ok(MUX_BOT_SHOWN.into()), |_| Ok(()), |exe, requirement| {
        looked = Some((exe.to_path_buf(), requirement.to_string()));
        Some("/icon.icns".into())
    })
    .unwrap();
    assert_eq!(app.icon.as_deref(), Some(std::path::Path::new("/icon.icns")));
    let (exe, requirement) = looked.unwrap();
    assert_eq!(exe, std::path::Path::new(MUX_EXE), "the kernel's path, never one read from codesign's text");
    assert_eq!(requirement, macos::signing_requirement("5LDXYW8B6T", "com.darklyenergized.darkmuxapp"));
    let mut asked = false;
    let unverified = from_codesign(Ok(MUX_BOT_SHOWN.into()), self_signed_verify, |_, _| {
        asked = true;
        Some("/icon.icns".into())
    });
    assert!(unverified.is_err());
    assert!(!asked, "no icon is read for an app whose signature did not hold");
}

/// The icon is a file inside the bundle's `Contents/Resources`: `.icns` is
/// added to a bare name, and a name with a path in it, `..`, or a link out
/// of `Resources` is refused.
#[test]
fn an_icon_must_resolve_inside_the_bundles_resources() {
    let dir = tempfile::tempdir().unwrap();
    let contents = dir.path().join("Mux Bot.app/Contents");
    let resources = contents.join("Resources");
    std::fs::create_dir_all(&resources).unwrap();
    std::fs::write(resources.join("AppIcon.icns"), b"icns").unwrap();
    std::fs::write(dir.path().join("outside.icns"), b"icns").unwrap();
    std::os::unix::fs::symlink(dir.path().join("outside.icns"), resources.join("Escape.icns")).unwrap();
    std::fs::create_dir_all(resources.join("x")).unwrap();
    let found = macos::icon_in_bundle(&contents, "AppIcon").unwrap();
    assert_eq!(found, resources.canonicalize().unwrap().join("AppIcon.icns"));
    assert!(macos::icon_in_bundle(&contents, "AppIcon.icns").is_some());
    for refused in ["../../outside.icns", "..", "x/../AppIcon", "Escape", "", "Missing"] {
        assert_eq!(macos::icon_in_bundle(&contents, refused), None, "{refused}");
    }
}

#[test]
fn an_executable_outside_an_app_bundle_has_no_bundle() {
    let exe = std::path::Path::new("/Applications/Mux Bot.app/Contents/MacOS/Mux Bot");
    assert_eq!(macos::bundle_contents(exe).as_deref(), Some(std::path::Path::new("/Applications/Mux Bot.app/Contents")));
    assert_eq!(macos::bundle_contents(std::path::Path::new("/usr/bin/python3")), None);
    assert_eq!(macos::bundle_contents(std::path::Path::new("/tmp/Contents/MacOS/x")), None, "not in a .app");
}

/// The icon's path is an argument too, after the message; with none, the
/// script shows the caution icon.
#[test]
fn the_icon_path_is_an_argument_and_caution_is_the_fallback() {
    let path = std::path::Path::new("/A \" & (do shell script \"id\") & \"/AppIcon.icns");
    let args = macos::allow_dialog_args("m", Some(path), 15);
    assert_eq!(args.last().unwrap(), &path.to_string_lossy());
    assert_eq!(args[args.len() - 2], "m");
    let script: Vec<&String> = args.iter().zip(args.iter().skip(1)).filter(|(f, _)| *f == "-e").map(|(_, l)| l).collect();
    assert!(script.iter().all(|l| !l.contains("do shell script")));
    assert!(script.iter().any(|l| l.contains("with icon caution")));
    // Return and Esc both mean Don't Allow: it is the default and the cancel
    // button, and a cancel (-128) is answered as Don't Allow.
    assert!(script.iter().all(|l| !l.contains("default button \"Allow\"")));
    assert!(script.iter().any(|l| l.contains("default button \"Don't Allow\" cancel button \"Don't Allow\"")));
    assert!(script.iter().any(|l| l.as_str() == "if n is -128 then return \"Don't Allow\""));
    assert_eq!(macos::allow_dialog_args("m", None, 15).len(), args.len() - 1, "no icon, no argument");
}

/// A tool that ends is read to its end; one that runs past the deadline is
/// killed and reads as none, so a wedged dialog never holds its caller.
#[test]
fn a_dialog_tool_is_read_or_killed_at_its_deadline() {
    let mut echo = std::process::Command::new("/bin/sh");
    echo.args(["-c", "echo Allow"]);
    let out = crate::output_within(echo, std::time::Duration::from_secs(10)).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "Allow");
    let mut slow = std::process::Command::new("/bin/sleep");
    slow.arg("30");
    let started = std::time::Instant::now();
    assert!(crate::output_within(slow, std::time::Duration::from_millis(200)).is_none());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
    assert!(crate::output_within(std::process::Command::new("/nonexistent/tool"), std::time::Duration::from_secs(1)).is_none());
}

#[test]
fn a_dialog_run_that_never_ended_is_no_dialog() {
    assert!(matches!(macos::dialog_run_answer(None), AllowAnswer::NoDialog(_)));
    assert!(matches!(linux::zenity_run_answer(None), AllowAnswer::NoDialog(_)));
    let mut sh = std::process::Command::new("/bin/sh");
    sh.args(["-c", "echo Allow; exit 1"]);
    let out = crate::output_within(sh, std::time::Duration::from_secs(10));
    assert_eq!(linux::zenity_run_answer(out.clone()), AllowAnswer::DontAllow, "zenity's exit decides, never its output");
    assert!(matches!(macos::dialog_run_answer(out), AllowAnswer::NoDialog(_)), "osascript failing is no dialog");
}

/// A graphical session is a display variable that is set and not empty;
/// without one, no dialog tool is used.
#[test]
fn a_dialog_tool_is_used_only_in_a_graphical_session() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let tool = dir.path().join("zenity");
    std::fs::write(&tool, "#!/bin/sh\n").unwrap();
    std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = Some(dir.path().as_os_str().to_owned());
    let set = |v: &str| Some(std::ffi::OsString::from(v));
    assert_eq!(linux::graphical_tool_in("zenity", [None, None], path.clone()), None);
    assert_eq!(linux::graphical_tool_in("zenity", [set(""), None], path.clone()), None, "empty is unset");
    assert_eq!(linux::graphical_tool_in("zenity", [set("wayland-1"), None], path.clone()), Some(tool.clone()));
    assert_eq!(linux::graphical_tool_in("zenity", [None, set(":0")], path), Some(tool));
    assert_eq!(linux::graphical_tool_in("zenity", [None, set(":0")], None), None);
}

/// (macOS) The real `codesign` leaves, against this test's own process:
/// the test binary is signed ad hoc by the linker, so it is read, never
/// verified against Apple's anchor, and never named.
#[cfg(target_os = "macos")]
#[test]
fn codesign_reads_this_ad_hoc_test_process_and_never_verifies_it() {
    let pid = std::process::id();
    let shown = macos::codesign_display(pid).unwrap();
    assert!(shown.contains("adhoc") || shown.contains("TeamIdentifier=not set"), "{shown}");
    assert!(macos::codesign_verify(pid, "anchor apple generic").is_err());
    assert!(MacPlatform.signed_app(pid).is_err());
    assert!(macos::codesign_display(u32::MAX).is_err(), "no such process");
}

/// A bundle verifier that passes, for an icon test not about verifying.
fn ok_bundle(_app: &std::path::Path, _requirement: &str) -> Result<(), String> {
    Ok(())
}

/// Every OS: the icon is looked up only for an executable inside an
/// `.app` bundle, only when `Info.plist` names one, and only when that file
/// is inside the bundle's `Resources`.
#[test]
fn an_icon_comes_only_from_a_bundle_that_names_one_inside_its_resources() {
    let dir = tempfile::tempdir().unwrap();
    let contents = dir.path().join("Mux Bot.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources")).unwrap();
    std::fs::write(contents.join("Resources/AppIcon.icns"), b"icns").unwrap();
    let exe = contents.join("MacOS/Mux Bot");
    let names = |name: &'static str| move |plist: &std::path::Path| {
        assert!(plist.ends_with("Contents/Info.plist"), "{plist:?}");
        Some(name.to_string())
    };
    let icon = macos::app_icon_by(&exe, "the requirement", |app, requirement| {
        assert!(app.ends_with("Mux Bot.app"), "{app:?}");
        assert_eq!(requirement, "the requirement");
        Ok(())
    }, names("AppIcon"))
    .unwrap();
    assert!(icon.ends_with("Contents/Resources/AppIcon.icns"), "{icon:?}");
    assert_eq!(macos::app_icon_by(&exe, "r", ok_bundle, |_| None), None, "Info.plist names no icon");
    assert_eq!(macos::app_icon_by(&exe, "r", ok_bundle, names("Missing")), None, "the named file is absent");
    assert_eq!(macos::app_icon_by(&exe, "r", ok_bundle, names("../../x")), None, "a path, not a file name");
    let unsigned = macos::app_icon_by(&exe, "r", |_, _| Err("code object is not signed at all".into()), names("AppIcon"));
    assert_eq!(unsigned, None, "a bundle that does not itself verify shows no icon");
    let loose = dir.path().join("bin/tool");
    assert_eq!(macos::app_icon_by(&loose, "r", ok_bundle, names("AppIcon")), None, "not inside an .app bundle");
}

/// (macOS) A bundle's icon is found from its executable through
/// `Info.plist`, by `plutil`.
#[cfg(target_os = "macos")]
#[test]
fn an_app_bundles_icon_is_found_from_its_executable() {
    let dir = tempfile::tempdir().unwrap();
    let contents = dir.path().join("Mux Bot.app/Contents");
    std::fs::create_dir_all(contents.join("MacOS")).unwrap();
    std::fs::create_dir_all(contents.join("Resources")).unwrap();
    std::fs::write(contents.join("Resources/AppIcon.icns"), b"icns").unwrap();
    std::fs::write(
        contents.join("Info.plist"),
        r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleIconFile</key><string>AppIcon</string></dict></plist>"#,
    )
    .unwrap();
    let exe = contents.join("MacOS/Mux Bot");
    let icon = macos::app_icon_by(&exe, "r", ok_bundle, macos::plist_icon_name).unwrap();
    assert!(icon.ends_with("Contents/Resources/AppIcon.icns"), "{icon:?}");
    let req = macos::signing_requirement("5LDXYW8B6T", "com.darklyenergized.darkmuxapp");
    assert_eq!(macos::app_icon(&exe, &req), None, "this unsigned bundle does not verify, so it shows no icon");
    std::fs::write(contents.join("Info.plist"), r#"<?xml version="1.0"?><plist version="1.0"><dict/></plist>"#).unwrap();
    assert_eq!(macos::app_icon_by(&exe, "r", ok_bundle, macos::plist_icon_name), None, "no icon named");
}

/// Text shown to someone deciding whether to trust an app keeps its order
/// and shows all it holds: bidirectional controls, zero-width and other
/// invisible format characters are dropped, control characters become spaces.
#[test]
fn display_safe_drops_reordering_and_invisible_characters() {
    for c in ['\u{202A}', '\u{202B}', '\u{202C}', '\u{202D}', '\u{202E}', '\u{2066}', '\u{2067}', '\u{2068}', '\u{2069}', '\u{200E}', '\u{200F}', '\u{061C}', '\u{200B}', '\u{200D}', '\u{2060}', '\u{FEFF}', '\u{00AD}', '\u{E0041}'] {
        assert_eq!(crate::display_safe(&format!("a{c}b")), "ab", "U+{:04X}", c as u32);
    }
    assert_eq!(crate::display_safe("a\nb\u{1b}c"), "a b c");
    assert_eq!(crate::display_safe("Mux Bot 4F7-K2Q é 日本"), "Mux Bot 4F7-K2Q é 日本", "ordinary text is kept");
}

/// Every container engine whose team signs its forwarder is never named as
/// an app, whatever its signature (the team ids read from each vendor's app).
#[test]
fn every_listed_container_engine_is_never_named() {
    for (team, engine) in [("HUAQ24HBR6", "OrbStack"), ("2Q6FHJR3H3", "Rancher Desktop"), ("HYSCB8KRL2", "Podman")] {
        let shown = format!("Identifier=x.y\nCodeDirectory v=20500 size=1 flags=0x10000(runtime) hashes=1+7 location=embedded\nAuthority=Developer ID Application: X ({team})\nTeamIdentifier={team}\n");
        let e = from_codesign(Ok(named(&shown)), |_| Ok(()), no_icon).unwrap_err();
        assert!(e.contains(engine) && e.contains("container traffic"), "{e}");
    }
}

/// (#3112) Which sandbox a host runs when `runtime.sandbox` is unset: a Mac
/// with `sandbox-exec` runs Seatbelt and needs no Docker; one without it
/// (never seen, but not assumed away) falls back to Docker.
#[test]
fn a_mac_with_sandbox_exec_defaults_to_seatbelt() {
    assert_eq!(crate::macos::default_sandbox_given(true), DefaultSandbox::Seatbelt);
    assert_eq!(crate::macos::default_sandbox_given(false), DefaultSandbox::Docker);
    let present = std::path::Path::new(SANDBOX_EXEC).is_file();
    assert_eq!(MacPlatform.default_sandbox(), crate::macos::default_sandbox_given(present));
}

/// (#3112) A Linux whose kernel can confine a process (Landlock ABI 6 and
/// unprivileged user namespaces) runs Landlock and needs no Docker; one that
/// cannot keeps Docker, as does every other OS.
#[test]
fn a_linux_that_can_confine_defaults_to_landlock() {
    assert_eq!(crate::linux::default_sandbox_given(true), DefaultSandbox::Landlock);
    assert_eq!(crate::linux::default_sandbox_given(false), DefaultSandbox::Docker);
    let confinable = LinuxPlatform.confine_support().refusal().is_none();
    assert_eq!(LinuxPlatform.default_sandbox(), crate::linux::default_sandbox_given(confinable));
    assert_eq!(OtherPlatform.default_sandbox(), DefaultSandbox::Docker);
}

#[test]
fn a_fake_answers_the_default_sandbox_the_test_set() {
    assert_eq!(FakePlatform::default().default_sandbox(), DefaultSandbox::Docker);
    let fake = FakePlatform { default_sandbox: DefaultSandbox::Seatbelt, ..FakePlatform::default() };
    assert_eq!(fake.default_sandbox(), DefaultSandbox::Seatbelt);
}

/// (#3112) `netstat -anv -p tcp`'s LISTEN rows on a port, read from the
/// kernel's own socket table (no walk over processes, which a fork-hopper
/// races): every address (`*`, by its family), IPv4 and IPv6 loopback, a
/// scoped address (`fe80::1%lo0`), and the owner's pid at the end of a
/// `process:pid` field whose name holds spaces. Another port or state is
/// not one. A header that is not netstat's, or an IPv4 LISTEN row on the
/// port that does not read, is no answer at all (`None`), never "nothing
/// listens". (#3112 review) netstat prints at most 16 characters of an IPv6
/// address: one that long is never read as an address (it may be cut), so
/// it is no address at all (`addr: None`), never loopback.
#[test]
fn netstat_listen_rows_on_a_port_parse_to_addresses_and_owners() {
    let head = "Active Internet connections (including servers)\nProto Recv-Q Send-Q  Local Address          Foreign Address        (state)          rxbytes      txbytes  rhiwat  shiwat          process:pid    state  options           gencnt    flags   flags1 usecnt rtncnt fltrs\n";
    let row = |proto: &str, local: &str, state: &str, who: &str| {
        format!("{proto}       0      0  {local}                *.*                    {state}                 0            0  131072  131072           {who}   00000 00000002 0000000000ff626a 00000000 00000800      1      0 000000\n")
    };
    let table = [
        head.to_string(),
        row("tcp4", "*.47996", "LISTEN", "Python:2900"),
        row("tcp6", "::1.47996", "LISTEN", "Python:2901"),
        row("tcp4", "127.0.0.1.47996", "LISTEN", "Grok Bot:82871"),
        row("tcp46", "*.47996", "LISTEN", "node:7"),
        row("tcp6", "fe80::1%lo0.47996", "LISTEN", "node:8"),
        row("tcp6", "fe80::1823:f45e:.47996", "LISTEN", "node:9"),
        row("tcp6", "2001:f40:974:5c8.47996", "LISTEN", "node:10"),
        row("tcp4", "127.0.0.1.47997", "LISTEN", "Python:2900"),
        "tcp4       0      0  127.0.0.1.47996        127.0.0.1.50000        ESTABLISHED         1 2 3 4 x:9 00 0 0 0 0 0 0 0\n".to_string(),
    ]
    .concat();
    let at = |addr: &str, text: &str, scope: Option<&str>, pid| PortListener { addr: Some(addr.parse().unwrap()), text: text.to_string(), scope: scope.map(str::to_string), pid: Some(pid) };
    let cut = |text: &str, pid| PortListener { addr: None, text: text.to_string(), scope: None, pid: Some(pid) };
    assert_eq!(
        crate::port_listeners_from_netstat(&table, 47996),
        Some(vec![
            at("0.0.0.0", "*", None, 2900),
            at("::1", "::1", None, 2901),
            at("127.0.0.1", "127.0.0.1", None, 82871),
            at("::", "*", None, 7),
            at("fe80::1", "fe80::1%lo0", Some("lo0"), 8),
            cut("fe80::1823:f45e:", 9),
            cut("2001:f40:974:5c8", 10),
        ])
    );
    assert_eq!(crate::port_listeners_from_netstat(&table, 1), Some(vec![]), "no listener on the port");
    assert_eq!(crate::port_listeners_from_netstat("not netstat\n", 1), None, "a header that is not netstat's");
    let broken = format!("{head}{}", row("tcp4", "garbage.47996x", "LISTEN", "x:1"));
    assert_eq!(crate::port_listeners_from_netstat(&broken, 47996), None, "a LISTEN row that does not read");
    let unparsed = format!("{head}{}", row("tcp4", "not-an-ip.47996", "LISTEN", "x:1"));
    assert_eq!(crate::port_listeners_from_netstat(&unparsed, 47996), None);
}

/// (#3112 review) Only a netstat that exited 0 answers; any other exit,
/// or none (killed at the bound), is no answer.
#[test]
fn only_a_netstat_that_succeeded_answers() {
    assert_eq!(crate::macos::netstat_answer(Some(0), "x"), Some("x".to_string()));
    for code in [Some(1), Some(2), None] {
        assert_eq!(crate::macos::netstat_answer(code, "x"), None, "{code:?}");
    }
}

/// (#3112) Who listens on a port and at which address, asked of this
/// host's strategy: a child listening on every address and one on the
/// loopback, on two ports, each read with its address and owner; a port
/// with none reads empty. An OS with no strategy cannot say; the fake
/// answers what was set.
#[test]
fn the_host_lists_a_ports_listeners_with_their_addresses() {
    let host = current();
    if host.tcp_port_listeners(1).is_none() {
        return; // no strategy reads sockets here
    }
    let mut child = std::process::Command::new("/usr/bin/python3")
        .args(["-c", "import socket,sys,time\na=socket.socket();a.bind(('0.0.0.0',0));a.listen()\nb=socket.socket();b.bind(('127.0.0.1',0));b.listen()\nprint(a.getsockname()[1],b.getsockname()[1],flush=True)\ntime.sleep(30)"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(&mut std::io::BufReader::new(child.stdout.take().unwrap()), &mut line).unwrap();
    let ports: Vec<u16> = line.split_whitespace().map(|p| p.parse().unwrap()).collect();
    let (wide, near) = (host.tcp_port_listeners(ports[0]), host.tcp_port_listeners(ports[1]));
    let _ = child.kill();
    let _ = child.wait();
    let pid = Some(child.id());
    let read = |l: Option<Vec<PortListener>>| l.map(|l| l.into_iter().map(|l| (l.addr, l.pid)).collect::<Vec<_>>());
    assert_eq!(read(wide), Some(vec![(Some("0.0.0.0".parse().unwrap()), pid)]));
    assert_eq!(read(near), Some(vec![(Some("127.0.0.1".parse().unwrap()), pid)]));
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    assert_eq!(host.tcp_port_listeners(free).map(|l| l.into_iter().filter(|l| l.pid != Some(std::process::id())).count()), Some(0));
    assert_eq!(OtherPlatform.tcp_port_listeners(1), None);
    let one = PortListener { addr: Some("0.0.0.0".parse().unwrap()), text: "0.0.0.0".into(), scope: None, pid: Some(7) };
    let fake = FakePlatform { port_listeners: vec![(5, vec![one.clone()])], ..Default::default() };
    assert_eq!((fake.tcp_port_listeners(5), fake.tcp_port_listeners(6)), (Some(vec![one]), Some(vec![])));
}

/// (#3112) `/proc/net/tcp{,6}`'s LISTEN rows as (address, inode), whatever
/// the port; an ESTABLISHED row is not one.
#[test]
fn proc_net_listen_rows_read_with_their_address() {
    let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
   0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41231 1 0000000000000000 100 0 0 10 0\n\
   1: 00000000:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 41232 1 0000000000000000 100 0 0 10 0\n\
   3: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 41234 1 0000000000000000 100 0 0 10 0\n";
    assert_eq!(
        crate::linux::listening_sockets(table),
        vec![("127.0.0.1:8080".parse().unwrap(), 41231), ("0.0.0.0:8081".parse().unwrap(), 41232)]
    );
}

/// (#3112 review) A bounded command's output is drained while it runs: one
/// that prints more than a pipe holds (netstat's table on a busy machine)
/// finishes, whole, instead of blocking on a full pipe until the bound
/// kills it.
#[test]
fn a_bounded_command_with_more_output_than_a_pipe_holds_finishes() {
    let mut cmd = std::process::Command::new("/bin/sh");
    cmd.args(["-c", "head -c 300000 /dev/zero; echo done >&2"]);
    let started = std::time::Instant::now();
    let out = crate::output_within(cmd, std::time::Duration::from_secs(3)).expect("it finished within its bound");
    assert_eq!(out.stdout.len(), 300_000);
    assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "done");
    assert!(started.elapsed() < std::time::Duration::from_secs(2), "{:?}", started.elapsed());
}

/// (#3561) The Node.js build darkmux fetches for a sandbox on the host:
/// the archive name's platform part for the two Mac and two Linux CPUs
/// Node publishes, and nothing for a CPU or OS it has no pin for.
#[test]
fn the_node_release_platform_names_the_official_build_for_the_cpu() {
    assert_eq!(node_release_platform_for(NodeOs::Darwin, "aarch64"), Some("darwin-arm64"));
    assert_eq!(node_release_platform_for(NodeOs::Darwin, "x86_64"), Some("darwin-x64"));
    assert_eq!(node_release_platform_for(NodeOs::Linux, "aarch64"), Some("linux-arm64"));
    assert_eq!(node_release_platform_for(NodeOs::Linux, "x86_64"), Some("linux-x64"));
    assert_eq!(node_release_platform_for(NodeOs::Linux, "riscv64"), None);
    assert_eq!(OtherPlatform.node_release_platform(), None);
    let fake = FakePlatform { node_release_platform: Some("linux-arm64"), ..Default::default() };
    assert_eq!(fake.node_release_platform(), Some("linux-arm64"));
    assert_eq!(FakePlatform::default().node_release_platform(), None);
    let arch = std::env::consts::ARCH;
    assert_eq!(MacPlatform.node_release_platform(), node_release_platform_for(NodeOs::Darwin, arch));
    assert_eq!(LinuxPlatform.node_release_platform(), node_release_platform_for(NodeOs::Linux, arch));
}

/// (#3559) A sandbox's processes in a process table of (pid, ppid, pgid,
/// sid): its leader, every process in the leader's group or session (an
/// orphan the leader left behind among them, one in a group of its own
/// included), and every descendant of any of those (one that moved to a
/// session of its own included). Nothing else: not another session's, not
/// the leader's own parent.
#[test]
fn a_sandboxs_processes_are_its_group_and_their_descendants() {
    let table = [
        (1, 0, 1, 1),         // launchd / init
        (50, 1, 50, 40),      // the leader's parent (darkmux)
        (100, 50, 100, 100),  // the leader
        (101, 100, 100, 100), // its child, in its group
        (102, 101, 102, 102), // a grandchild that started a session of its own
        (105, 102, 105, 102), // and its child
        (103, 1, 100, 100),   // an orphan still in the group
        (104, 103, 104, 100), // the orphan's child, in a group of its own
        (106, 1, 106, 100),   // an orphan in a group of its own, still in the session
        (200, 1, 200, 200),   // another program
        (201, 200, 200, 200), // and its child
    ];
    assert_eq!(crate::sandbox_members(100, &table), vec![100, 101, 102, 103, 104, 105, 106]);
    // A leader that has exited still has its group and what descends from it.
    let gone: Vec<_> = table.iter().copied().filter(|(p, _, _, _)| *p != 100).collect();
    assert_eq!(crate::sandbox_members(100, &gone), vec![100, 101, 102, 103, 104, 105, 106]);
    // launchd or init is never a sandbox's leader: nothing would be outside it.
    assert_eq!(crate::sandbox_members(1, &table), Vec::<u32>::new());
    assert_eq!(crate::sandbox_members(0, &table), Vec::<u32>::new());
}

/// (#3559) `/proc/<pid>/stat` as (pid, ppid, pgrp, session): the name, in
/// parentheses, may hold spaces and parentheses itself.
#[test]
fn a_proc_stat_line_reads_as_its_pid_parent_and_group() {
    assert_eq!(crate::linux::stat_ids("1234 (my (odd) prog) S 1200 1234 1230 0 -1 4194560"), Some((1234, 1200, 1234, 1230)));
    assert_eq!(crate::linux::stat_ids("7 (sh) R 1 7 7"), Some((7, 1, 7, 7)));
    assert_eq!(crate::linux::stat_ids("7 (sh) R 1 7"), None);
    assert_eq!(crate::linux::stat_ids(""), None);
    // A zombie holds no files (its `fd` directory is root's, so unreadable).
    assert!(crate::linux::stat_says_exited("9 (a (Z) b) Z 1 9 9"));
    assert!(crate::linux::stat_says_exited("9 (sh) X 1 9 9"));
    assert!(!crate::linux::stat_says_exited("9 (Z) S 1 9 9"));
    assert!(!crate::linux::stat_says_exited(""));
}

/// (#3559) One socket's `proc_pidfdinfo(PROC_PIDFDSOCKETINFO)` record (a
/// `struct socket_fdinfo`, read at the offsets `<sys/proc_info.h>` gives
/// on macOS 26) as a TCP listener: its port (network order), its address
/// (IPv4 or IPv6, a dual-stack `*` as `::`), a link-local address's scope
/// (embedded in the address, KAME style, or given as the interface index)
/// by name. A socket in another TCP state, or not TCP, is no listener.
#[test]
fn a_socket_fdinfo_record_reads_as_a_listener() {
    use crate::macos::libproc::{listener_in_fdinfo, SOCKET_FDINFO_SIZE};
    let record = |kind: i32, state: i32, vflag: u8, port: u16, addr: &[u8], at: usize, ifindex: u16| {
        let mut b = vec![0u8; SOCKET_FDINFO_SIZE];
        b[256..260].copy_from_slice(&kind.to_ne_bytes());
        b[344..348].copy_from_slice(&state.to_ne_bytes());
        b[288] = vflag;
        b[268..270].copy_from_slice(&port.to_be_bytes());
        b[at..at + addr.len()].copy_from_slice(addr);
        b[340..342].copy_from_slice(&ifindex.to_ne_bytes());
        b
    };
    let names = |i: u32| (i == 1).then(|| "lo0".to_string());
    let read = |b: &[u8]| listener_in_fdinfo(b, 42, names).map(|(port, l)| (port, l.addr, l.scope, l.pid));
    let v4 = |a: [u8; 4]| record(2, 1, 1, 8080, &a, 324, 0);
    assert_eq!(read(&v4([0, 0, 0, 0])), Some((8080, Some("0.0.0.0".parse().unwrap()), None, Some(42))));
    assert_eq!(read(&v4([127, 0, 0, 1])), Some((8080, Some("127.0.0.1".parse().unwrap()), None, Some(42))));
    let v6 = |a: std::net::Ipv6Addr, vflag: u8, ifindex: u16| record(2, 1, vflag, 4100, &a.octets(), 312, ifindex);
    assert_eq!(read(&v6("::".parse().unwrap(), 3, 0)), Some((4100, Some("::".parse().unwrap()), None, Some(42))));
    assert_eq!(read(&v6("::1".parse().unwrap(), 2, 0)), Some((4100, Some("::1".parse().unwrap()), None, Some(42))));
    let lo0 = Some("lo0".to_string());
    assert_eq!(read(&v6("fe80:1::1".parse().unwrap(), 2, 0)), Some((4100, Some("fe80::1".parse().unwrap()), lo0.clone(), Some(42))));
    assert_eq!(read(&v6("fe80::1".parse().unwrap(), 2, 1)), Some((4100, Some("fe80::1".parse().unwrap()), lo0, Some(42))));
    // A scope with no name stays unnamed (never read as the loopback's).
    assert_eq!(read(&v6("fe80:9::1".parse().unwrap(), 2, 0)), Some((4100, Some("fe80::1".parse().unwrap()), None, Some(42))));
    // ESTABLISHED (4), not TCP (SOCKINFO_IN, 1), no address family flag.
    assert_eq!(read(&record(2, 4, 1, 8080, &[0; 4], 324, 0)), None);
    assert_eq!(read(&record(1, 1, 1, 8080, &[0; 4], 324, 0)), None);
    assert_eq!(read(&record(2, 1, 0, 8080, &[0; 4], 324, 0)), None);
    assert_eq!(read(&[0u8; 100]), None);
}

/// Kills a fixture's process group and any process that left it, when the
/// test ends (a panic included), so no listener outlives its test.
#[cfg(unix)]
struct Fixture {
    group: u32,
    others: Vec<u32>,
    child: std::process::Child,
}

#[cfg(unix)]
impl Drop for Fixture {
    fn drop(&mut self) {
        for pid in &self.others {
            // SAFETY: a signal to a pid this test started.
            unsafe { libc::kill(*pid as i32, libc::SIGKILL) };
        }
        // SAFETY: a signal to the process group this test started.
        unsafe { libc::kill(-(self.group as i32), libc::SIGKILL) };
        let _ = self.child.wait();
    }
}

/// (#3559) The TCP listeners a sandbox's processes hold, asked of this
/// host's strategy, against a real process tree in a group of its own: its
/// leader's on every address, on the loopback and on IPv6's every address;
/// one a descendant opened after moving to a session of its own; one whose
/// opener handed it to its own child and exited (the socket table still
/// names the dead opener; the holder is what is read). The test's own
/// listener on every address is another process's and never one of them.
/// A group holding nothing reads empty. An OS with no strategy cannot say;
/// the fake answers what was set.
#[cfg(unix)]
#[test]
fn the_host_lists_the_listeners_a_sandboxs_processes_hold() {
    use std::io::BufRead;
    use std::os::unix::process::CommandExt;
    let host = current();
    let foreign = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let foreign_port = foreign.local_addr().unwrap().port();
    let script = r#"import os, socket, sys, time
def listen(family, host):
    s = socket.socket(family); s.bind((host, 0)); s.listen(); return s
a = listen(socket.AF_INET, '0.0.0.0'); b = listen(socket.AF_INET, '127.0.0.1')
own = [a, b]
try:
    c = listen(socket.AF_INET6, '::'); cp = c.getsockname()[1]; own.append(c)
except OSError:
    cp = 0
r, w = os.pipe()
if os.fork() == 0:
    [o.close() for o in own]
    os.setsid(); s = listen(socket.AF_INET, '0.0.0.0')
    os.write(w, b'left %d %d\n' % (s.getsockname()[1], os.getpid())); time.sleep(30); os._exit(0)
if os.fork() == 0:
    [o.close() for o in own]
    h = listen(socket.AF_INET, '0.0.0.0')
    if os.fork() == 0:
        os.write(w, b'kept %d %d\n' % (h.getsockname()[1], os.getpid())); time.sleep(30); os._exit(0)
    os._exit(0)
got = b''
while got.count(b'\n') < 2:
    got += os.read(r, 100)
sys.stdout.write('own %d %d %d %d\n' % (a.getsockname()[1], b.getsockname()[1], cp, os.getpid()) + got.decode()); sys.stdout.flush()
time.sleep(30)
"#;
    let mut child = std::process::Command::new("/usr/bin/python3")
        .args(["-c", script])
        .stdout(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let leader = child.id();
    let mut fixture = Fixture { group: leader, others: Vec::new(), child };
    let mut lines = std::io::BufReader::new(out).lines();
    let mut said = std::collections::HashMap::new();
    while said.len() < 3 {
        let line = lines.next().expect("the fixture said what it listens on").unwrap();
        let words: Vec<u32> = line.split_whitespace().skip(1).map(|w| w.parse().unwrap()).collect();
        said.insert(line.split_whitespace().next().unwrap().to_string(), words);
    }
    let (own, left, kept) = (&said["own"], &said["left"], &said["kept"]);
    fixture.others.push(left[1]);
    let any4: std::net::IpAddr = "0.0.0.0".parse().unwrap();
    let mut want = vec![
        (own[0] as u16, any4, leader),
        (own[1] as u16, "127.0.0.1".parse().unwrap(), leader),
        (left[0] as u16, any4, left[1]),
        (kept[0] as u16, any4, kept[1]),
    ];
    if own[2] != 0 {
        want.push((own[2] as u16, "::".parse().unwrap(), leader));
    }
    want.sort();
    if host.id() == OtherPlatform.id() {
        return; // no strategy reads sockets here
    }
    let held = host.tcp_listeners_held_by(leader).expect("the host reads its own user's processes");
    let mut held: Vec<_> = held.into_iter().map(|(p, l)| (p, l.addr.unwrap(), l.pid.unwrap())).collect();
    held.sort();
    assert_eq!(held, want);
    assert!(!held.iter().any(|(p, _, _)| *p == foreign_port), "another process's listener counted");
    let mut idle = std::process::Command::new("/bin/sleep").arg("30").process_group(0).spawn().unwrap();
    let nothing = host.tcp_listeners_held_by(idle.id());
    let _ = idle.kill();
    let _ = idle.wait();
    assert_eq!(nothing, Some(Vec::new()));
    drop(fixture);
    drop(foreign);
    assert_eq!(OtherPlatform.tcp_listeners_held_by(1), None);
    let one = PortListener { addr: Some(any4), text: "0.0.0.0".into(), scope: None, pid: Some(7) };
    let fake = FakePlatform { held_listeners: vec![(5, vec![(80, one.clone())])], ..Default::default() };
    assert_eq!((fake.tcp_listeners_held_by(5), fake.tcp_listeners_held_by(6)), (Some(vec![(80, one)]), Some(vec![])));
    *fake.held_listeners_reads.lock().unwrap() = [false, true, false].into();
    let reads: Vec<bool> = (0..4).map(|_| fake.tcp_listeners_held_by(6).is_some()).collect();
    assert_eq!(reads, [false, true, false, true], "the scripted reads, then answers");
}

/// (#3559) Whether a listener is on the loopback, and how a message names
/// it: `127.x`, `::1`, a v4-mapped loopback and an address scoped to the
/// loopback interface are; every address, a LAN address, and an address
/// the platform could not read whole are not.
#[test]
fn a_listener_on_the_loopback_is_told_from_one_beyond_it() {
    let at = |addr: &str, scope: Option<&str>| PortListener {
        addr: Some(addr.parse().unwrap()),
        text: addr.to_string(),
        scope: scope.map(str::to_string),
        pid: None,
    };
    for near in [at("127.0.0.1", None), at("127.8.9.1", None), at("::1", None), at("::ffff:127.0.0.1", None), at("fe80::1", Some("lo0")), at("fe80::1", Some("lo"))] {
        assert!(near.on_loopback(), "{near:?}");
    }
    let cut = PortListener { addr: None, text: "::1:2:3:4:5:6:7".into(), scope: None, pid: None };
    for far in [at("0.0.0.0", None), at("::", None), at("192.168.1.5", None), at("fe80::1", Some("en0")), at("fe80::1", None), cut.clone()] {
        assert!(!far.on_loopback(), "{far:?}");
    }
    assert_eq!(at("0.0.0.0", None).named_at(4100), "0.0.0.0:4100");
    assert_eq!(at("::", None).named_at(4100), "[::]:4100");
    assert_eq!(cut.named_at(4100), "[::1:2:3:4:5:6:7…]:4100 (an IPv6 address netstat cut short; not a loopback one)");
}

/// (#3559) A libproc read is the whole record (or, sized by the kernel, any
/// bytes, or none with no error), nothing because what it read is gone
/// (`ESRCH`, `EBADF`), or no answer: a short record, or any other error
/// (`EPERM`), never "none".
#[test]
fn a_libproc_read_is_whole_gone_or_no_answer() {
    use crate::macos::libproc::{libproc_read, LibprocRead};
    assert_eq!(libproc_read(792, Some(792), 0), LibprocRead::Read);
    assert_eq!(libproc_read(64, None, 0), LibprocRead::Read);
    assert_eq!(libproc_read(0, None, 0), LibprocRead::Read);
    assert_eq!(libproc_read(0, Some(792), 3), LibprocRead::Gone);
    assert_eq!(libproc_read(0, Some(792), 9), LibprocRead::Gone);
    assert_eq!(libproc_read(0, Some(792), 38), LibprocRead::Gone, "ENOTSOCK: the descriptor reused for a file");
    assert_eq!(libproc_read(0, None, 3), LibprocRead::Gone);
    assert_eq!(libproc_read(400, Some(792), 0), LibprocRead::Unreadable);
    assert_eq!(libproc_read(0, Some(792), 0), LibprocRead::Unreadable);
    assert_eq!(libproc_read(0, Some(792), 1), LibprocRead::Unreadable);
    assert_eq!(libproc_read(-1, None, 1), LibprocRead::Unreadable);
}

/// (#3559) A command started in a session of its own leads it (its
/// session and group are its own pid), and what it runs cannot move into a
/// process group of another session (this test's). Set together with
/// `process_group(0)`, the spawn fails rather than quietly staying in the
/// caller's session.
#[cfg(unix)]
#[test]
fn a_command_in_a_session_of_its_own_cannot_join_another_sessions_group() {
    use std::os::unix::process::CommandExt;
    // SAFETY: plain queries of this process.
    let ours = unsafe { libc::getpgrp() };
    let mut cmd = std::process::Command::new("/usr/bin/perl");
    cmd.args(["-e", &format!("$| = 1; print(setpgrp(0, {ours}) ? 'joined' : 'refused'); sleep 5")]);
    cmd.stdout(std::process::Stdio::piped());
    crate::in_a_session_of_its_own(&mut cmd);
    let mut child = cmd.spawn().unwrap();
    let pid = child.id() as i32;
    std::thread::sleep(std::time::Duration::from_millis(200));
    // SAFETY: queries of the child this test started.
    let (sid, group) = unsafe { (libc::getsid(pid), libc::getpgid(pid)) };
    let _ = child.kill();
    let out = child.wait_with_output().unwrap();
    assert_eq!((sid, group), (pid, pid), "it leads a session and a group of its own");
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("refused"), "{:?}", String::from_utf8_lossy(&out.stdout));
    let mut both = std::process::Command::new("/usr/bin/true");
    crate::in_a_session_of_its_own(&mut both);
    both.process_group(0);
    assert!(both.spawn().is_err(), "a group leader cannot start a session: the spawn must fail");
}

/// (#3559) At the call sites, a refused read is no answer, never "none":
/// macOS's per-socket reads (`proc_pidfdinfo`) and its descriptor list
/// (`PROC_PIDLISTFDS`), and Linux's `fd` directory. A descriptor or process
/// that is gone since it was listed (closed, reused for a file, exited, a
/// zombie) holds nothing.
#[test]
fn a_refused_read_at_the_call_site_is_no_answer() {
    use crate::macos::libproc::{listeners_in_records, socket_fds_in, SOCKET_FDINFO_SIZE};
    let full = SOCKET_FDINFO_SIZE as i32;
    let names = |_: u32| None;
    // A listener record: TCP, LISTEN, IPv4 every address, port 80.
    let listener = |info: &mut [u8]| {
        info[256..260].copy_from_slice(&2i32.to_ne_bytes());
        info[344..348].copy_from_slice(&1i32.to_ne_bytes());
        info[288] = 1;
        info[268..270].copy_from_slice(&80u16.to_be_bytes());
    };
    let read = |answers: Vec<(i32, i32)>| {
        let mut answers = answers.into_iter();
        move |_fd: i32, info: &mut [u8]| {
            let (got, errno) = answers.next().unwrap();
            if got == SOCKET_FDINFO_SIZE as i32 {
                listener(info);
            }
            (got, errno)
        }
    };
    let ports = |held: Option<Vec<(u16, PortListener)>>| held.map(|h| h.into_iter().map(|(p, _)| p).collect::<Vec<_>>());
    assert_eq!(ports(listeners_in_records(7, &[3, 4], read(vec![(full, 0), (full, 0)]), names)), Some(vec![80, 80]));
    for gone in [9, 38, 3] {
        assert_eq!(ports(listeners_in_records(7, &[3, 4], read(vec![(0, gone), (full, 0)]), names)), Some(vec![80]), "errno {gone}");
    }
    for refused in [1, 12, 0] {
        assert_eq!(ports(listeners_in_records(7, &[3, 4], read(vec![(full, 0), (0, refused)]), names)), None, "errno {refused}");
    }
    assert_eq!(ports(listeners_in_records(7, &[3], read(vec![(400, 0)]), names)), None, "a short record");
    let entries = [(3, 2), (4, 1), (5, 2), (6, 2)];
    assert_eq!(socket_fds_in(24, 0, &entries), Some(vec![3, 5]), "only the listed entries, only sockets");
    assert_eq!(socket_fds_in(0, 0, &entries), Some(vec![]));
    assert_eq!(socket_fds_in(0, 3, &entries), Some(vec![]), "an exited process");
    assert_eq!(socket_fds_in(0, 1, &entries), None, "a refused list");

    use crate::linux::socket_inodes_in;
    let live = || Some("9 (sh) S 1 9 9".to_string());
    let zombie = || Some("9 (sh) Z 1 9 9".to_string());
    assert_eq!(socket_inodes_in(Ok(vec!["socket:[41]".into(), "/dev/null".into()]), live), Some(vec![41]));
    assert_eq!(socket_inodes_in(Err(std::io::ErrorKind::NotFound), live), Some(vec![]));
    assert_eq!(socket_inodes_in(Err(std::io::ErrorKind::PermissionDenied), live), None);
    assert_eq!(socket_inodes_in(Err(std::io::ErrorKind::PermissionDenied), zombie), Some(vec![]));
    assert_eq!(socket_inodes_in(Err(std::io::ErrorKind::PermissionDenied), || None), Some(vec![]), "gone by the time its stat is read");
}

/// (#3559) Live, at the call site: another user's process (launchd's or
/// init's, pid 1) cannot have its open files read by this one, and that is
/// no answer. Skipped when the tests run as root.
#[cfg(unix)]
#[test]
fn another_users_open_files_are_no_answer() {
    // SAFETY: a plain query of this process.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    if cfg!(target_os = "macos") {
        assert_eq!(crate::macos::listeners_held_by_pid(1), None);
    }
    if cfg!(target_os = "linux") {
        assert_eq!(crate::linux::socket_inodes_held_by(1), None);
    }
}

/// A minimal ELF file shaped like a real one: a header, then program
/// headers in a real binary's order (`PT_PHDR`, `PT_INTERP` naming `interp`
/// when given, then a `PT_LOAD` covering the whole file), in 64- or 32-bit
/// form and either byte order. Only `PT_INTERP` names an interpreter: a
/// reader that took another header for it would read the table or the ELF
/// header as a path.
fn an_elf(is64: bool, big: bool, interp: Option<&str>) -> Vec<u8> {
    let (ehsize, phentsize) = if is64 { (64usize, 56usize) } else { (52, 32) };
    let phnum = 2 + usize::from(interp.is_some());
    let interp_at = ehsize + phentsize * phnum;
    let mut b = vec![0u8; interp_at];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = if is64 { 2 } else { 1 };
    b[5] = if big { 2 } else { 1 };
    b[6] = 1;
    let put = |b: &mut Vec<u8>, at: usize, v: u64, width: usize| {
        let bytes = if big { v.to_be_bytes() } else { v.to_le_bytes() };
        let part = if big { &bytes[8 - width..] } else { &bytes[..width] };
        b[at..at + width].copy_from_slice(part);
    };
    if is64 {
        put(&mut b, 0x20, ehsize as u64, 8);
        put(&mut b, 0x36, phentsize as u64, 2);
        put(&mut b, 0x38, phnum as u64, 2);
    } else {
        put(&mut b, 0x1C, ehsize as u64, 4);
        put(&mut b, 0x2A, phentsize as u64, 2);
        put(&mut b, 0x2C, phnum as u64, 2);
    }
    // One program header: type, file offset, file size.
    let header = |b: &mut Vec<u8>, n: usize, kind: u64, offset: u64, size: u64| {
        let ph = ehsize + n * phentsize;
        put(b, ph, kind, 4);
        if is64 {
            put(b, ph + 8, offset, 8);
            put(b, ph + 32, size, 8);
        } else {
            put(b, ph + 4, offset, 4);
            put(b, ph + 16, size, 4);
        }
    };
    header(&mut b, 0, 6, ehsize as u64, (phentsize * phnum) as u64);
    if let Some(path) = interp {
        header(&mut b, 1, 3, interp_at as u64, path.len() as u64 + 1);
        b.extend_from_slice(path.as_bytes());
        b.push(0);
    }
    let len = b.len() as u64;
    header(&mut b, phnum - 1, 1, 0, len);
    b
}

/// (#3561 re-review) A Linux machine's C library is its `/bin/sh`'s ELF
/// interpreter: musl where it is `ld-musl-*` (Alpine's busybox, also with
/// `gcompat` installed beside it), glibc for any other (Debian with the
/// `musl` package, NixOS's store path). Read from 64- and 32-bit files in
/// either byte order; a file with no interpreter, cut short or not ELF
/// reads as having none, which is glibc. A musl machine has no official
/// Node build darkmux pins (they link glibc), so it names none; a Mac has
/// no such question.
#[test]
fn a_linux_c_library_is_its_shells_elf_interpreter() {
    let busybox = an_elf(true, false, Some("/lib/ld-musl-x86_64.so.1"));
    assert_eq!(elf_interpreter(&busybox).as_deref(), Some("/lib/ld-musl-x86_64.so.1"));
    assert_eq!(c_library_of_interpreter(elf_interpreter(&busybox).as_deref()), CLibrary::Musl);
    for (elf, want) in [
        (an_elf(true, false, Some("/lib/ld-musl-aarch64.so.1")), CLibrary::Musl),
        (an_elf(true, false, Some("/lib64/ld-linux-x86-64.so.2")), CLibrary::Glibc),
        (an_elf(true, false, Some("/nix/store/0abc-glibc-2.39-52/lib/ld-linux-x86-64.so.2")), CLibrary::Glibc),
        (an_elf(false, true, Some("/lib/ld-musl-mips.so.1")), CLibrary::Musl),
        (an_elf(false, false, Some("/lib/ld-linux-armhf.so.3")), CLibrary::Glibc),
        (an_elf(true, true, Some("/lib64/ld64.so.2")), CLibrary::Glibc),
    ] {
        assert_eq!(c_library_of_interpreter(elf_interpreter(&elf).as_deref()), want, "{:?}", elf_interpreter(&elf));
    }
    let static_sh = an_elf(true, false, None);
    assert_eq!(elf_interpreter(&static_sh), None, "a static shell has no interpreter");
    let cut = &busybox[..busybox.len() - 10];
    assert_eq!(elf_interpreter(cut), None, "an interpreter cut short is not read");
    assert_eq!(elf_interpreter(&busybox[..40]), None, "a header cut short is not read");
    assert_eq!(elf_interpreter(b"#!/bin/busybox sh\n"), None, "not ELF");
    let mut no_magic = busybox.clone();
    no_magic[1] = b'X';
    assert_eq!(elf_interpreter(&no_magic), None, "a file without the ELF magic is not ELF, whatever follows");
    assert_eq!(elf_interpreter(&[]), None);
    let mut lying = busybox.clone();
    lying[0x38] = 0xff;
    lying[0x39] = 0xff;
    assert_eq!(elf_interpreter(&lying), None, "a program header table past the file is not read");
    assert_eq!(c_library_of_interpreter(None), CLibrary::Glibc, "no interpreter: glibc, the default");
    assert_eq!(linux_node_release_platform("x86_64", CLibrary::Musl), None);
    assert_eq!(linux_node_release_platform("aarch64", CLibrary::Musl), None);
    assert_eq!(linux_node_release_platform("x86_64", CLibrary::Glibc), Some("linux-x64"));
    assert_eq!(MacPlatform.c_library(), None);
    assert_eq!(OtherPlatform.c_library(), None);
    let fake = FakePlatform { c_library: Some(CLibrary::Musl), ..Default::default() };
    assert_eq!(fake.c_library(), Some(CLibrary::Musl));
    assert_eq!(FakePlatform::default().c_library(), None);
}

/// (#3561 re-review) `LinuxPlatform` itself reads its shell (here a temp
/// file standing in for `/bin/sh`): Alpine's busybox, `gcompat` or not,
/// is musl and names no build; a glibc shell names the glibc build; a shell
/// it cannot read is glibc.
#[test]
fn the_linux_platform_reads_its_own_shell() {
    let dir = tempfile::TempDir::new().unwrap();
    let sh = dir.path().join("sh");
    let read = |bytes: Option<Vec<u8>>| {
        let _ = std::fs::remove_file(&sh);
        if let Some(b) = bytes {
            std::fs::write(&sh, b).unwrap();
        }
        crate::linux::with_shell(sh.clone(), || (LinuxPlatform.c_library(), LinuxPlatform.node_release_platform()))
    };
    let glibc_build = node_release_platform_for(NodeOs::Linux, std::env::consts::ARCH);
    assert_eq!(read(Some(an_elf(true, false, Some("/lib/ld-musl-x86_64.so.1")))), (Some(CLibrary::Musl), None));
    assert_eq!(read(Some(an_elf(true, false, Some("/lib64/ld-linux-x86-64.so.2")))), (Some(CLibrary::Glibc), glibc_build));
    assert_eq!(read(None), (Some(CLibrary::Glibc), glibc_build), "no shell to read: glibc");
}

/// (#3561 re-review) This CI host's real `/bin/sh` (Arch on the Omarchy
/// runner, glibc) reads as glibc.
#[cfg(target_os = "linux")]
#[test]
fn this_linux_hosts_shell_reads_as_glibc() {
    assert_eq!(LinuxPlatform.c_library(), Some(CLibrary::Glibc));
}
