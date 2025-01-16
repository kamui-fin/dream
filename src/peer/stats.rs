use log::info;

use crate::piece::KB_PER_BLOCK;

pub const STATS_WINDOW_SEC: usize = 5;

pub struct PeerStats {
    // track the sum of the speeds to calculate kbps every n-seconds
    pub current_speeds_sum: f32,
    // track kb downloaded in current window
    pub current_blocks_downloaded: u32,

    // tracks the overall download speed across ALL previous windows (to be used for choking algo)
    pub total_avg_kbps: f32,
    // tracks the total number downloaded across ALL previous windows
    pub total_kb: u32,
}

impl PeerStats {
    pub fn init_stats() -> Self {
        Self {
            current_speeds_sum: 0.0,
            current_blocks_downloaded: 0,
            total_avg_kbps: 0.0,
            total_kb: 0,
        }
    }

    pub fn add_new_speed(&mut self, new_speed: &f32) {
        self.current_speeds_sum += new_speed;
        self.current_blocks_downloaded += 1;
    }

    pub fn update_overalls(&mut self) {
        // get the average time it took for one kb (sec per kb)
        let average_time = self.current_speeds_sum / (self.current_blocks_downloaded as f32);

        // convert secs per block to kbps
        let current_window_kbps = (KB_PER_BLOCK) as f32 / average_time;

        // take weighted moving average and weigh the current kbps more than the historical average
        self.total_avg_kbps = (0.125 * current_window_kbps) + (0.875 * self.total_avg_kbps);
        info!("total_avg_kbps: {:#?}", self.total_avg_kbps);

        // track historical kb downloaded
        self.total_kb += self.current_blocks_downloaded * KB_PER_BLOCK;

        // reset current window
        self.current_speeds_sum = 0.0;
        self.current_blocks_downloaded = 0;
    }
}
