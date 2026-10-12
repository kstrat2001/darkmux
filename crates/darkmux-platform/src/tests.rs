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

/// Clusters are listed fastest first, the order macOS's `hw.perflevelN`
/// uses: Asahi's P cores have capacity 1024, its E cores less.
#[test]
fn cpu_clusters_come_from_cpufreq_policies_fastest_first() {
    let policies = vec![
        (vec![0, 1, 2, 3], Some(571)),
        (vec![4, 5, 6, 7], Some(1024)),
    ];
    let clusters = crate::linux::clusters_from_policies(policies);
    assert_eq!(
        clusters,
        vec![
            CpuClusterInfo {
                name: "Performance".into(),
                logical_cpus: 4
            },
            CpuClusterInfo {
                name: "Efficiency".into(),
                logical_cpus: 4
            },
        ]
    );
    // One cluster (a homogeneous CPU) is just "CPU".
    let single = crate::linux::clusters_from_policies(vec![(vec![0, 1], Some(1024))]);
    assert_eq!(
        single,
        vec![CpuClusterInfo {
            name: "CPU".into(),
            logical_cpus: 2
        }]
    );
    assert!(crate::linux::clusters_from_policies(vec![]).is_empty());
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
        assert_eq!(
            clusters.iter().map(|c| c.logical_cpus).sum::<usize>(),
            ticks.len()
        );
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

/// Each cluster's current frequency, in the same fastest-first order as
/// `cpu_clusters`, from the policy's `scaling_cur_freq` (kHz).
#[test]
fn cluster_mhz_follows_the_cluster_order() {
    let policies = vec![
        (vec![0, 1, 2, 3], Some(571), Some(600_000)),
        (vec![4, 5, 6, 7], Some(1024), Some(1_968_000)),
    ];
    assert_eq!(crate::linux::cluster_mhz_from_policies(policies), vec![Some(1968), Some(600)]);
    assert_eq!(crate::linux::cluster_mhz_from_policies(vec![(vec![0], Some(1024), None)]), vec![None]);
}

#[test]
fn this_linux_machine_reports_a_frequency_per_cluster() {
    if std::env::consts::OS == "linux" {
        assert_eq!(LinuxPlatform.cpu_cluster_mhz().len(), LinuxPlatform.cpu_clusters().len());
    }
    assert!(MacPlatform.cpu_cluster_mhz().is_empty() && OtherPlatform.cpu_cluster_mhz().is_empty());
}
