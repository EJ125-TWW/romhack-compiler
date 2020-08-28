use super::virtual_file_system::{Directory, Node};
use super::{consts::*, FstEntry, FstNodeType};
use byteorder::{WriteBytesExt, BE};
use failure::{err_msg, Error, ResultExt};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::fs;

pub fn write_iso<W>(mut writer: W, root: &Directory) -> Result<(), Error>
where
    W: Write + Seek,
{
    let (sys_index, sys_dir) = root
        .children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.as_directory().map(|d| (i, d)))
        .find(|&(_, d)| d.name == "&&systemdata")
        .ok_or_else(|| err_msg("The virtual file system contains no &&systemdata folder"))?;

    let header = sys_dir
        .children
        .iter()
        .filter_map(|c| c.as_file())
        .find(|f| f.name == "iso.hdr")
        .ok_or_else(|| err_msg("The &&systemdata folder contains no iso.hdr"))?;
    writer.write_all(&header.data)?;

    let apploader = sys_dir
        .children
        .iter()
        .filter_map(|c| c.as_file())
        .find(|f| f.name == "AppLoader.ldr")
        .ok_or_else(|| err_msg("The &&systemdata folder contains no AppLoader.ldr"))?;
    writer.write_all(&apploader.data)?;

    let dol_offset_without_padding = header.data.len() + apploader.data.len();
    let dol_offset =
        (dol_offset_without_padding + (DOL_ALIGNMENT - 1)) / DOL_ALIGNMENT * DOL_ALIGNMENT;

    for _ in dol_offset_without_padding..dol_offset {
        writer.write_all(&[0])?;
    }

    let dol = sys_dir
        .children
        .iter()
        .filter_map(|c| c.as_file())
        .find(|f| f.name.ends_with(".dol"))
        .ok_or_else(|| err_msg("The &&systemdata folder contains no dol file"))?;
    writer.write_all(&dol.data)?;

    let fst_list_offset_without_padding = dol_offset + dol.data.len();
    let fst_list_offset =
        (fst_list_offset_without_padding + (FST_ALIGNMENT - 1)) / FST_ALIGNMENT * FST_ALIGNMENT;

    for _ in fst_list_offset_without_padding..fst_list_offset {
        writer.write_all(&[0])?;
    }

    let mut fst_len = 12;
    for (_, node) in root
        .children
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != sys_index)
    {
        fst_len = calculate_fst_len(fst_len, node);
    }

    for _ in 0..fst_len {
        // TODO Seems suboptimal
        // Should not be a problem with BufWriter though
        writer.write_all(&[0])?;
    }

    let root_fst = FstEntry {
        kind: FstNodeType::Directory,
        ..Default::default()
    };

    // Placeholder FST entry for the root
    let mut output_fst = vec![root_fst];
    let mut fst_name_bank = Vec::new();

    for (_, node) in root
        .children
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != sys_index)
    {
        do_output_prep(node, &mut output_fst, &mut fst_name_bank, &mut writer, 0)?;
    }

    // Add actual root FST entry
    output_fst[0].file_size_next_dir_index = output_fst.len();

    writer.seek(SeekFrom::Start(fst_list_offset as u64))?;

    for entry in &output_fst {
        writer.write_u8(entry.kind as u8)?;
        writer.write_u8(0)?;
        writer.write_u16::<BE>(entry.file_name_offset as u16)?;
        writer.write_i32::<BE>(entry.file_offset_parent_dir as i32)?;
        writer.write_i32::<BE>(entry.file_size_next_dir_index as i32)?;
    }

    writer.write_all(&fst_name_bank)?;

    writer.seek(SeekFrom::Start(OFFSET_DOL_OFFSET as u64))?;
    writer.write_u32::<BE>(dol_offset as u32)?;
    writer.write_u32::<BE>(fst_list_offset as u32)?;
    writer.write_u32::<BE>(fst_len as u32)?;
    writer.write_u32::<BE>(fst_len as u32)?;

    Ok(())
}

pub fn write_fs(path: PathBuf, root: &Directory) -> Result<(), Error> {
    fs::create_dir_all(&path);
    // TODO Write the function
    let (root_index, root_dir, _) = write_data_dir(None, "&&rootdata", root, &path)?;

    write_data_file(&path, "cert.bin", "cert.bin", &root_dir);
    write_data_file(&path, "h3.bin", "h3.bin", &root_dir);
    write_data_file(&path, "ticket.bin", "ticket.bin", &root_dir);
    write_data_file(&path, "tmd.bin", "tmd.bin", &root_dir);

    let (sys_index, sys_dir, sys_path) = write_data_dir(Some("sys"), "&&systemdata", root, &path)?;

    write_data_file(&sys_path, "bi2.bin", "iso.hdr", &sys_dir);
    write_data_file(&sys_path, "apploader.img", "AppLoader.ldr", &sys_dir);
    write_data_file(&sys_path, "main.dol", "Start.dol", &sys_dir);
    write_data_file(&sys_path, "boot.bin", "Game.toc", &sys_dir);
    write_data_file(&sys_path, "fst.bin", "fst.bin", &sys_dir);

    let (disc_index, disc_dir, disc_path) = write_data_dir(Some("disc"), "&&discdata", root, &path)?;

    write_data_file(&disc_path, "header.bin", "header.bin", &disc_dir);
    write_data_file(&disc_path, "region.bin", "region.bin", &disc_dir);

    let mut files_path = path.clone();
    files_path.push("files");
    fs::create_dir_all(&files_path);
    for (_, node) in root
        .children
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != sys_index)
    {
        write_files_recursive(node, &files_path)?;
    }

    Ok(())
}

fn write_data_dir<'a>(dir_fs_name: Option<&str>, dir_given_name: &str, parent_dir: &'a Directory<'a>, path: &PathBuf) -> Result<(usize, &'a Directory<'a>, PathBuf), Error> {
    let (dir_index, dir) = parent_dir
        .children
        .iter()
        .enumerate()
        .filter_map(|(i, c)| c.as_directory().map(|d| (i, d)))
        .find(|&(_, d)| d.name == dir_given_name)
        .ok_or_else(|| err_msg(format!("The {} folder contains no {}", parent_dir.name, dir_given_name)))?;
    let mut dir_path = path.clone();
    if let Some(dir_fs_name) = dir_fs_name {
        dir_path.push(dir_fs_name);
        fs::create_dir_all(&dir_path);
    }
    Ok((dir_index, dir, dir_path))
}

fn write_data_file(dir_path: &PathBuf, fs_name: &str, given_name: &str, dir: &Directory) -> Result<(), Error> {
    let file = dir
        .children
        .iter()
        .filter_map(|c| c.as_file())
        .find(|f| f.name == given_name)
        .ok_or_else(|| err_msg(format!("The {} folder contains no {}", dir.name, given_name)))?;
    let mut file_path = dir_path.clone();
    file_path.push(fs_name);
    fs::File::create(&file_path).context(format!("Couldn't open file \"{:?}\"", file_path.to_str()))?
        .write(&file.data).context(format!("Couldn't write to file \"{:?}\"", file_path.to_str()))?;
    Ok(())
}

fn calculate_fst_len(mut cur_value: usize, node: &Node) -> usize {
    match *node {
        Node::Directory(ref dir) => {
            cur_value += 12 + dir.name.len() + 1;

            for child in &dir.children {
                cur_value = calculate_fst_len(cur_value, child);
            }
        }
        Node::File(ref file) => {
            cur_value += 12 + file.name.len() + 1;
        }
    }
    cur_value
}

fn do_output_prep<W>(
    node: &Node,
    output_fst: &mut Vec<FstEntry>,
    fst_name_bank: &mut Vec<u8>,
    writer: &mut W,
    mut cur_parent_dir_index: usize,
) -> Result<(), Error>
where
    W: Write + Seek,
{
    match *node {
        Node::Directory(ref dir) => {
            let fst_ent = FstEntry {
                kind: FstNodeType::Directory,
                file_name_offset: fst_name_bank.len(),
                file_offset_parent_dir: cur_parent_dir_index,
                ..Default::default()
            };

            fst_name_bank.extend_from_slice(dir.name.as_bytes());
            fst_name_bank.push(0);

            cur_parent_dir_index = output_fst.len();

            let this_dir_index = cur_parent_dir_index;

            output_fst.push(fst_ent); // Placeholder for this dir

            for child in &dir.children {
                do_output_prep(
                    child,
                    output_fst,
                    fst_name_bank,
                    writer,
                    cur_parent_dir_index,
                )?;
            }

            let dir_end_index = output_fst.len();
            output_fst[this_dir_index].file_size_next_dir_index = dir_end_index;
        }
        Node::File(ref file) => {
            let mut fst_ent = FstEntry {
                kind: FstNodeType::File,
                file_size_next_dir_index: file.data.len(),
                file_name_offset: fst_name_bank.len(),
                ..Default::default()
            };

            fst_name_bank.extend_from_slice(file.name.as_bytes());
            fst_name_bank.push(0);

            let pos = writer.seek(SeekFrom::Current(0))?;
            let new_pos = pos + (32 - (pos % 32)) % 32;
            writer.seek(SeekFrom::Start(new_pos))?;

            fst_ent.file_offset_parent_dir = new_pos as usize;

            writer.write_all(&file.data)?;

            for _ in 0..(32 - (file.data.len() % 32)) % 32 {
                writer.write_all(&[0])?;
            }

            output_fst.push(fst_ent);
        }
    }

    Ok(())
}

fn write_files_recursive(
    node: &Node,
    parent_path: &PathBuf
) -> Result<(), Error>
{
    match *node {
        Node::Directory(ref dir) => {
            let mut dir_path = parent_path.clone();
            dir_path.push(&dir.name);
            if !dir_path.exists() {
                fs::create_dir_all(&dir_path).context(format!("Couldn't create directory \"{:?}\"", dir_path.to_str()))?;
            }
            for (_, sub_node) in dir
                .children
                .iter()
                .enumerate()
            {
                write_files_recursive(sub_node, &dir_path)?;
            }
        }
        Node::File(ref file) => {
            let mut file_path = parent_path.clone();
            file_path.push(&file.name);
            fs::File::create(&file_path).context(format!("Couldn't open file \"{:?}\"", file_path.to_str()))?
                .write(&file.data).context(format!("Couldn't write to file \"{:?}\"", file_path.to_str()))?;
        }
    }
    Ok(())
}