use anyhow::Result;
use crossbeam::sync::WaitGroup;
use dcmrig_rs::*;
use dicom::{
    core::{
        chrono::NaiveDate,
        value::{DicomDate, DicomDateTime, DicomTime},
        DataElement, VR,
    },
    dicom_value,
    dictionary_std::tags,
    object::{open_file, FileDicomObject, InMemDicomObject},
};
use rayon::prelude::*;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use tracing::{debug, error, info};

pub fn dicom_anon(
    source_path: PathBuf,
    destination_path: PathBuf,
    anon_prefix: String,
) -> Result<()> {
    info!(
        "Anonymizing the data for >> SOURCE: {} | DESTINATION: {} | ANON PREFIX: {}",
        source_path.display(),
        destination_path.display(),
        &anon_prefix
    );

    // Set up required variables
    let (all_files, total_len, pb) = preprocessing_setup(&source_path, &destination_path)?;
    let failed_case: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let non_dcm_cases: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
    let anon_id_tracker: Arc<Mutex<HashMap<String, String>>> = Arc::new(Mutex::new(HashMap::new()));
    let wg = WaitGroup::new();

    // Main Loop
    all_files
        .par_iter()
        .enumerate()
        .for_each(|(_index, working_path)| {
            if let Ok(dcm_obj) = open_file(working_path.path()) {
                let anon_id_clone = Arc::clone(&anon_id_tracker);
                anon_each_dcm_file(
                    &dcm_obj,
                    &destination_path,
                    anon_id_clone,
                    &anon_prefix,
                    wg.clone(),
                )
                .unwrap_or_else(|_| {
                    let mut map = failed_case.lock().expect("Failed to lock mutex");
                    *map += 1;
                    error!(
                        "Can't ANON {:#?} Copying to FAILED_CASES directory",
                        &working_path.file_name()
                    );
                    failed_case_copy(&working_path.clone().into_path(), &destination_path)
                        .expect("Failed to copy file to FAILED_CASES directory");
                });
            } else {
                let mut map = non_dcm_cases.lock().expect("Failed to lock mutex");
                *map += 1;
                copy_non_dicom_files(&working_path, &destination_path).unwrap_or_else(|_| {
                    error!("Can't copy non dicom file {:#?}", &working_path.file_name())
                })
            }
            pb.inc(1);
        });
    pb.finish();
    print_status(
        total_len,
        *failed_case.lock().expect("Failed to lock mutex"),
        *non_dcm_cases.lock().expect("Failed to lock mutex"),
        "Anon".to_string(),
    )?;
    wg.wait();
    info!("DICOM Anon complete!");
    Ok(())
}

fn anon_each_dcm_file(
    dcm_obj: &FileDicomObject<InMemDicomObject>,
    destination_path: &PathBuf,
    map_clone: Arc<Mutex<HashMap<std::string::String, std::string::String>>>,
    anon_prefix: &String,
    wg: WaitGroup,
) -> Result<()> {
    let patient_id = dcm_obj.element_by_name("PatientID")?.to_str()?.to_string();
    let mut map = map_clone.lock().expect("Failed to lock mutex");
    match map.get(&patient_id) {
        Some(_) => (),
        None => {
            let anon_id: String = if anon_prefix.len() == 0 {
                gen_id()
            } else {
                format!("{anon_prefix}_{}", gen_id())
            };
            map.insert(patient_id.clone(), anon_id);
            debug!("New AnonID for: {}", patient_id);
        }
    }
    let patient_anon_id = map
        .get(&patient_id)
        .expect("Failed to index Hashmap")
        .to_string();
    let mut new_dicom_object = mask_tags_with_id(dcm_obj.clone(), patient_anon_id)?;
    new_dicom_object = dicom_anon_date_time(new_dicom_object)?;
    let dicom_tags_values: HashMap<String, String> = get_sanitized_tag_values(&new_dicom_object)?;
    let new_dp = destination_path.clone();
    let dcm_obj_clone = new_dicom_object.clone();
    rayon::spawn(move || {
        let file_name =
            generate_dicom_file_name(&dicom_tags_values, "ANON".to_string()).expect("msg");
        let dir_path = generate_dicom_file_path(dicom_tags_values, &new_dp).expect("msg");
        let full_path = check_if_dup_exists(format!("{}/{}", dir_path, file_name));
        debug!("Saving file: {} to: {}", file_name, dir_path);
        dcm_obj_clone
            .write_to_file(full_path)
            .expect("Failed to save file");
        drop(wg);
    });
    Ok(())
}

fn dicom_anon_date_time(
    dcm_obj: FileDicomObject<InMemDicomObject>,
) -> Result<FileDicomObject<InMemDicomObject>> {
    // Setting Up primitives
    let time_str = "090000".to_string();
    let date_str = "19000101".to_string();
    let d_date = DicomDate::try_from(&NaiveDate::parse_from_str(&date_str, "%Y%m%d")?)?;

    let hr: u8 = time_str[0..2].to_string().parse()?;
    let min: u8 = time_str[2..4].to_string().parse()?;
    let sec: u8 = time_str[4..6].to_string().parse()?;
    let d_time = DicomTime::from_hms(hr, min, sec)?;

    let dicom_date_data = dicom_value!(Date, d_date);
    let dicom_time_data = dicom_value!(Time, d_time);
    let dicom_date_time =
        dicom_value!(DateTime, DicomDateTime::from_date_and_time(d_date, d_time)?);

    let date_deleted_dcm_obj = mask_all_vr(dcm_obj.clone(), VR::DA, dicom_date_data)?;
    let time_deleted_dcm_obj = mask_all_vr(date_deleted_dcm_obj.clone(), VR::TM, dicom_time_data)?;
    let mut datetime_deleted_dcm_obj =
        mask_all_vr(time_deleted_dcm_obj.clone(), VR::DT, dicom_date_time)?;
    datetime_deleted_dcm_obj.put(DataElement::new(
        tags::PATIENT_AGE,
        VR::AS,
        dicom_value!(Strs, ["099Y".to_string()]),
    ));
    datetime_deleted_dcm_obj.put(DataElement::new(
        tags::PATIENT_SEX,
        VR::CS,
        dicom_value!(Strs, ["O".to_string()]),
    ));
    Ok(datetime_deleted_dcm_obj)
}
