#!/usr/bin/sh

args="${@:2}"

qemu-system-x86_64 \
        -cpu qemu64,apic,fsgsbase,rdtscp,xsave,fxsr \
        -enable-kvm \
        -display none \
        -smp 4 \
        -m 1G \
        -serial stdio \
        -kernel loader/target/x86_64-unknown-hermit-loader/debug/rusty-loader \
        -initrd $1 \
        -netdev tap,id=net0,ifname=tap0,script=no,downscript=no,vhost=on \
        -device virtio-net-pci,netdev=net0,disable-legacy=on \
        -append "$args" \
        -monitor telnet:127.0.0.1:55555,server,nowait

