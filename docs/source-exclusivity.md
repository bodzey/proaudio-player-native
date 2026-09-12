# Source exclusivity

Spotify, AirPlay, MPD and DLNA are transport endpoints, not independent gain stages. The source arbiter selects one unmuted programme source on MUSIC and keeps exactly one sink-input from that source at unity. Losing sources and stale duplicate sink-inputs are muted, so normal programme playback cannot sum multiple full-scale transports into the MUSIC bus.
