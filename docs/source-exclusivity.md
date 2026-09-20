# Source exclusivity

Spotify, AirPlay, network audio, MPD and DLNA are transport endpoints, not independent gain stages. The source arbiter selects one unmuted programme source on MUSIC and keeps exactly one sink-input from that source at unity. Losing sources and stale duplicate sink-inputs are muted and suppressed until they disappear or reconnect, so normal programme playback cannot sum multiple full-scale transports into the MUSIC bus. Where a protocol provides a Stop operation the arbiter requests it; it never terminates a receiver by trusting a process ID advertised in stream metadata.
