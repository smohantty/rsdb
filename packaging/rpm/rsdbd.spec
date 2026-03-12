%global debug_package %{nil}
%global rsdbd_arch %{?rsdbd_arch}%{!?rsdbd_arch:aarch64}

Name:           rsdbd
Version:        %{?rsdbd_version}%{!?rsdbd_version:0.1.0}
Release:        1
Summary:        RSDB root daemon
License:        MIT OR Apache-2.0
BuildArch:      %{rsdbd_arch}
AutoReqProv:    no

Source0:        rsdbd
Source1:        rsdbd.service
Source2:        rsdbd.env

%description
RSDB root daemon for Tizen devices. Installs the daemon binary, systemd unit,
and default runtime environment file.

%prep

%build

%install
install -Dpm0755 %{SOURCE0} %{buildroot}/usr/bin/rsdbd
install -Dpm0644 %{SOURCE1} %{buildroot}/usr/lib/systemd/system/rsdbd.service
install -Dpm0644 %{SOURCE2} %{buildroot}/etc/rsdbd.env

%pre
if [ "$1" -gt 1 ] && command -v systemctl >/dev/null 2>&1; then
    systemctl stop rsdbd.service >/dev/null 2>&1 || true
fi

%post
if command -v install >/dev/null 2>&1; then
    install -d -m 0755 /var/log >/dev/null 2>&1 || true
    touch /var/log/rsdbd.log >/dev/null 2>&1 || true
    chmod 0644 /var/log/rsdbd.log >/dev/null 2>&1 || true
fi
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || true
    systemctl enable rsdbd.service >/dev/null 2>&1 || true
    if systemctl restart rsdbd.service >/dev/null 2>&1; then
        rsdbd_active=0
        for _ in 1 2 3 4 5; do
            if systemctl is-active --quiet rsdbd.service; then
                rsdbd_active=1
                break
            fi
            sleep 1
        done
        if [ "$rsdbd_active" -eq 1 ]; then
            echo "rsdbd.service restarted and is active"
        else
            echo "warning: rsdbd.service did not become active within 5 seconds after install"
            echo "check status: systemctl status rsdbd.service --no-pager -l"
            echo "check journal: journalctl -u rsdbd.service -n 100 --no-pager"
            echo "check file log: tail -n 100 /var/log/rsdbd.log"
        fi
    else
        echo "warning: systemctl restart rsdbd.service failed during install"
        echo "check status: systemctl status rsdbd.service --no-pager -l"
        echo "check journal: journalctl -u rsdbd.service -n 100 --no-pager"
        echo "check file log: tail -n 100 /var/log/rsdbd.log"
    fi
fi

%preun
if [ "$1" -eq 0 ] && command -v systemctl >/dev/null 2>&1; then
    systemctl stop rsdbd.service >/dev/null 2>&1 || true
    systemctl disable rsdbd.service >/dev/null 2>&1 || true
fi

%postun
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload >/dev/null 2>&1 || true
fi

%files
%attr(0755,root,root) /usr/bin/rsdbd
%attr(0644,root,root) /usr/lib/systemd/system/rsdbd.service
%config(noreplace) %attr(0644,root,root) /etc/rsdbd.env
