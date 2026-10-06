// SPDX-License-Identifier: GPL-3.0-or-later
//! OpenCV 5 split calibration, geometry, and corner detection into modules.
//! Use the dependency's version macros so OpenCV 4 builds keep their existing API.
opencv::opencv_branch_5! {
    pub use opencv::geometry as geometry;
    pub use opencv::geometry::{recover_pose_3 as recover_pose_triangulated, find_homography as find_homography_ext};
    pub use opencv::features::good_features_to_track;
    pub use opencv::objdetect::{CALIB_CB_MARKER, find_chessboard_corners_sb, estimate_chessboard_sharpness};
    pub use opencv::calib::{CALIB_RECOMPUTE_EXTRINSIC as Fisheye_CALIB_RECOMPUTE_EXTRINSIC, CALIB_FIX_SKEW as Fisheye_CALIB_FIX_SKEW};
    pub use opencv::core::Point2f as CalibrationPoint;
}
opencv::not_opencv_branch_5! {
    pub use opencv::calib3d as geometry;
    pub use opencv::calib3d::{recover_pose_triangulated, find_homography_ext};
    pub use opencv::imgproc::good_features_to_track;
    pub use opencv::calib3d::{CALIB_CB_MARKER, find_chessboard_corners_sb, estimate_chessboard_sharpness, Fisheye_CALIB_RECOMPUTE_EXTRINSIC, Fisheye_CALIB_FIX_SKEW};
    pub use opencv::core::Point2f as CalibrationPoint;
}
