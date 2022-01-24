#!/usr/bin/sh

ip tuntap add dev tap0 mode tap
ip addr add 10.0.5.1/24 broadcast 10.0.5.255 dev tap0
ip link set dev tap0 up
echo 1 | tee /proc/sys/net/ipv4/conf/tap0/proxy_arp
